use crate::function::{self, FunctionDeclaration, JsonSchema};
use anyhow::{Context, Result, bail};
use argc::{ChoiceValue, CommandValue, FlagOptionValue};
use indexmap::IndexMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub fn generate_bash_declarations(
    mut tool_file: File,
    tools_file_path: &Path,
    file_name: &str,
) -> Result<Vec<FunctionDeclaration>> {
    let mut src = String::new();
    tool_file
        .read_to_string(&mut src)
        .with_context(|| format!("Failed to load script at '{tool_file:?}'"))?;

    debug!("Building script at '{tool_file:?}'");
    let (build_script, declarations) = build_bash_tool(&src, file_name)
        .with_context(|| format!("Failed to build script '{file_name}'"))?;
    function::write_file_atomic(tools_file_path, &build_script, Some(0o755))
        .with_context(|| format!("Failed to write built script to '{tools_file_path:?}'"))?;

    Ok(declarations)
}

/// Builds the argc script and derives its declarations without touching the
/// filesystem, so callers can inspect a tool source without rewriting it.
pub fn build_bash_tool(src: &str, file_name: &str) -> Result<(String, Vec<FunctionDeclaration>)> {
    let build_script = argc::build(
        src,
        "",
        env::var("TERM_WIDTH").ok().and_then(|v| v.parse().ok()),
    )?;
    let build_script = allow_empty_required_values(&build_script);
    let build_script = keep_attached_option_values(&build_script);

    let command_value = argc::export(&build_script, file_name)
        .with_context(|| format!("Failed to parse script '{file_name}'"))?;
    if command_value.subcommands.is_empty() {
        let function_declaration =
            command_to_function_declaration(&command_value).ok_or_else(|| {
                anyhow::format_err!("Tool definition missing or empty description: {file_name}")
            })?;
        Ok((build_script, vec![function_declaration]))
    } else {
        let mut declarations = vec![];
        for subcommand in &command_value.subcommands {
            if subcommand.name.starts_with('_') && subcommand.name != "_instructions" {
                continue;
            }

            if let Some(mut function_declaration) = command_to_function_declaration(subcommand) {
                function_declaration.agent = true;
                declarations.push(function_declaration);
            } else {
                bail!(
                    "Tool definition missing or empty description: {} {}",
                    file_name,
                    subcommand.name
                );
            }
        }

        Ok((build_script, declarations))
    }
}

fn command_to_function_declaration(cmd: &CommandValue) -> Option<FunctionDeclaration> {
    if cmd.describe.is_empty() {
        return None;
    }

    Some(FunctionDeclaration {
        name: underscore(&cmd.name),
        description: cmd.describe.clone(),
        parameters: parse_parameters_schema(&cmd.flag_options),
        agent: false,
    })
}

fn underscore(s: &str) -> String {
    s.replace('-', "_")
}

/// argc's generated required-param check uses `-z "${!name:-}"`, which
/// conflates "not provided" with "provided but empty", so a required option
/// passed an explicit empty string (e.g. `fs_write --content=''` to create an
/// empty file) is rejected as "required arguments were not provided". The
/// JSON schema we advertise to models treats `required` as *presence*, so
/// rewrite the check to a set-ness test to keep runtime behavior consistent
/// with the schema. Applied post-build so it survives every regeneration.
fn allow_empty_required_values(build_script: &str) -> String {
    build_script.replace(
        r#"if [[ -z "${!name:-}" ]]; then"#,
        r#"if [[ -z "${!name+x}" ]]; then"#,
    )
}

/// argc 1.24.0's `take_args.sh` extracts an attached option value with
/// `${_argc_item##*=}`, which strips the longest prefix through the *last*
/// `=`, so `--pattern='*=*.nothing'` yields `*.nothing` and
/// `--summary='before (count == 0) after'` yields ` 0) after`. Our shims
/// always pass values in attached form, so any value containing `=` is
/// silently truncated. The key side already uses `%%=*` (first `=`); fix the
/// value side to match with `#*=`. Applied post-build so it survives every
/// regeneration.
fn keep_attached_option_values(build_script: &str) -> String {
    build_script.replace(
        r#"_argc_take_args_values=("${_argc_item##*=}")"#,
        r#"_argc_take_args_values=("${_argc_item#*=}")"#,
    )
}

fn schema_ty(t: &str) -> JsonSchema {
    JsonSchema {
        type_value: Some(t.to_string()),
        description: None,
        properties: None,
        items: None,
        any_of: None,
        enum_value: None,
        default: None,
        required: None,
    }
}

fn with_description(mut schema: JsonSchema, describe: &str) -> JsonSchema {
    if !describe.is_empty() {
        schema.description = Some(describe.to_string());
    }
    schema
}

fn with_enum(mut schema: JsonSchema, choice: &Option<ChoiceValue>) -> JsonSchema {
    if let Some(ChoiceValue::Values(values)) = choice
        && !values.is_empty()
    {
        schema.enum_value = Some(values.clone());
    }
    schema
}

fn parse_property(flag: &FlagOptionValue) -> JsonSchema {
    let mut schema = if flag.flag {
        schema_ty("boolean")
    } else if flag.multiple_occurs {
        let mut arr = schema_ty("array");
        arr.items = Some(Box::new(schema_ty("string")));
        arr
    } else if flag.notations.first().map(|s| s.as_str()) == Some("INT") {
        schema_ty("integer")
    } else if flag.notations.first().map(|s| s.as_str()) == Some("NUM") {
        schema_ty("number")
    } else {
        schema_ty("string")
    };

    schema = with_description(schema, &flag.describe);
    schema = with_enum(schema, &flag.choice);
    schema
}

fn parse_parameters_schema(flags: &[FlagOptionValue]) -> JsonSchema {
    let filtered = flags.iter().filter(|f| f.id != "help" && f.id != "version");
    let mut props: IndexMap<String, JsonSchema> = IndexMap::new();
    let mut required: Vec<String> = Vec::new();

    for f in filtered {
        let key = underscore(&f.id);
        if f.required {
            required.push(key.clone());
        }
        props.insert(key, parse_property(f));
    }

    JsonSchema {
        type_value: Some("object".to_string()),
        description: None,
        properties: Some(props),
        items: None,
        any_of: None,
        enum_value: None,
        default: None,
        required: Some(required),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_argc_required_check_to_setness_test() {
        let src = "# @describe test tool\n# @option --content! The content\nmain() { :; }\n";
        let built = argc::build(src, "", None).expect("argc build failed");
        assert!(
            built.contains(r#"if [[ -z "${!name:-}" ]]; then"#),
            "argc changed its generated required-param template; update allow_empty_required_values()"
        );

        let fixed = allow_empty_required_values(&built);

        assert!(fixed.contains(r#"if [[ -z "${!name+x}" ]]; then"#));
        assert!(!fixed.contains(r#"if [[ -z "${!name:-}" ]]; then"#));
    }

    #[test]
    fn rewrites_argc_attached_value_strip_to_first_equals_test() {
        let src = "# @describe test tool\n# @option --v The value\nmain() { :; }\n";
        let built = argc::build(src, "", None).expect("argc build failed");
        assert!(
            built.contains(r#"_argc_take_args_values=("${_argc_item##*=}")"#),
            "argc changed its take_args template; update keep_attached_option_values()"
        );

        let fixed = keep_attached_option_values(&built);

        assert!(fixed.contains(r#"_argc_take_args_values=("${_argc_item#*=}")"#));
        assert!(!fixed.contains(r#"_argc_take_args_values=("${_argc_item##*=}")"#));
    }

    #[cfg(unix)]
    #[test]
    fn built_tool_keeps_equals_in_attached_option_values_test() {
        use std::fs;
        use std::process::Command;

        if which::which("bash").is_err() {
            eprintln!("skipping: bash not available");
            return;
        }

        let src = "#!/usr/bin/env bash\n# @describe t\n# @option --v! value\nmain() { printf '%s' \"$argc_v\" >> \"$LLM_OUTPUT\"; }\n";
        let path = crate::utils::temp_file("bash-attached", ".sh");
        fs::write(&path, src).expect("failed to write temp script");
        generate_bash_declarations(File::open(&path).unwrap(), &path, "t").expect("build failed");

        let run = |args: &[&str]| -> String {
            let out = crate::utils::temp_file("bash-attached-out", ".txt");
            let status = Command::new("bash")
                .arg(&path)
                .args(args)
                .env("LLM_OUTPUT", &out)
                .status()
                .expect("failed to run built script");
            assert!(status.success(), "built script failed for {args:?}");
            let value = fs::read_to_string(&out).unwrap();
            fs::remove_file(&out).unwrap();
            value
        };

        assert_eq!(run(&["--v=a=b=c"]), "a=b=c");
        assert_eq!(run(&["--v", "a=b=c"]), "a=b=c");
        assert_eq!(run(&["--v=plain"]), "plain");

        fs::remove_file(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn second_build_does_not_rewrite_tool_file_test() {
        use std::fs;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let src = "# @describe test tool\n# @option --content! The content\nmain() { :; }\n";
        let path = crate::utils::temp_file("bash-build", ".sh");
        fs::write(&path, src).expect("failed to write temp script");

        let declarations = generate_bash_declarations(File::open(&path).unwrap(), &path, "test")
            .expect("first build failed");
        assert!(!declarations.is_empty());
        let first_ino = fs::metadata(&path).unwrap().ino();

        let declarations = generate_bash_declarations(File::open(&path).unwrap(), &path, "test")
            .expect("second build failed");
        assert!(!declarations.is_empty());

        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(
            metadata.ino(),
            first_ino,
            "second build rewrote an already-built tool file"
        );
        assert_eq!(metadata.permissions().mode() & 0o777, 0o755);

        fs::remove_file(&path).unwrap();
    }
}
