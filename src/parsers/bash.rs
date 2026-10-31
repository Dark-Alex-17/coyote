use crate::function::{self, FunctionDeclaration, JsonSchema};
use anyhow::{Context, Result, bail};
use argc::{ChoiceValue, CommandValue, FlagOptionValue};
use indexmap::IndexMap;
use std::borrow::Cow;
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
    let build_script = tolerate_empty_arrays_under_nounset(&build_script);

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

/// argc 1.24.0's generated code passes several arrays on with a bare
/// `"${a[@]}"`, and those arrays are empty on ordinary calls: `_argc_run`
/// with no positionals, or a subcommand with no required option, env or
/// tool while a sibling declares one; `take_args.sh` when a delimited option
/// value is empty (`--x=`); `split_positionals.sh` when a delimited
/// positional is empty or has no positionals before or after it; the
/// `_argc_validate_choices` call for an option or a multi-value positional
/// with choices when its value splits to nothing. Every agent tool runs
/// under `set -u` (from `.shared/utils.sh`), and bash before 4.4,
/// including the 3.2 that macOS ships, treats an empty `"${a[@]}"` as an
/// unbound variable, so the call dies before the function runs. Rewrite each
/// one to `${a[@]+"${a[@]}"}`, which expands to nothing for an empty array on
/// every bash. The `{var_name}+=(...)` and `{var_name}=(...)` assignments are
/// matched by their suffix because the variable name varies per parameter.
/// An option's choice check always validates `_argc_take_args_values`, after
/// the `)"` that closes its list of choices, so it is listed here; a
/// positional's validates `argc_<name>` and is handled by
/// `nounset_safe_choice_line`. Only the generated block, from
/// `# ARGC-BUILD {` through `# ARGC-BUILD }`, is rewritten, so the generic
/// template lines cannot match hand-written tool code before or after it.
/// The `argc__positionals` slices in `split_positionals.sh` and the loop over
/// `_argc_take_args_values` in `take_args.sh` stay bare because argc only
/// reaches them with a non-empty array. Applied post-build so it survives
/// every regeneration.
const EMPTY_ARRAY_EXPANSIONS: [(&str, &str); 12] = [
    (
        r#"$argc__fn "${argc__positionals[@]}""#,
        r#"$argc__fn ${argc__positionals[@]+"${argc__positionals[@]}"}"#,
    ),
    (
        r#"_argc_require_params "error: the following required arguments were not provided:" "${_argc_required_flag_options[@]}""#,
        r#"_argc_require_params "error: the following required arguments were not provided:" ${_argc_required_flag_options[@]+"${_argc_required_flag_options[@]}"}"#,
    ),
    (
        r#"_argc_require_params "error: the following required environments were not provided:" "${_argc_required_envs[@]}""#,
        r#"_argc_require_params "error: the following required environments were not provided:" ${_argc_required_envs[@]+"${_argc_required_envs[@]}"}"#,
    ),
    (
        r#"_argc_require_tools "${_argc_tools[@]}""#,
        r#"_argc_require_tools ${_argc_tools[@]+"${_argc_tools[@]}"}"#,
    ),
    (
        r#"arr+=("${values[@]}")"#,
        r#"arr+=(${values[@]+"${values[@]}"})"#,
    ),
    (
        r#"_argc_take_args_values=("${arr[@]}")"#,
        r#"_argc_take_args_values=(${arr[@]+"${arr[@]}"})"#,
    ),
    (
        r#"+=("${_argc_take_args_values[@]}")"#,
        r#"+=(${_argc_take_args_values[@]+"${_argc_take_args_values[@]}"})"#,
    ),
    (
        r#")" "${_argc_take_args_values[@]}""#,
        r#")" ${_argc_take_args_values[@]+"${_argc_take_args_values[@]}"}"#,
    ),
    (
        r#"for item in "${split_values[@]}"; do"#,
        r#"for item in ${split_values[@]+"${split_values[@]}"}; do"#,
    ),
    (
        r#"_argc_split_positionals_values+=("${values[@]}")"#,
        r#"_argc_split_positionals_values+=(${values[@]+"${values[@]}"})"#,
    ),
    (
        r#"argc__positionals=("${heads[@]}" "${_argc_split_positionals_values[@]}" "${tails[@]}")"#,
        r#"argc__positionals=(${heads[@]+"${heads[@]}"} ${_argc_split_positionals_values[@]+"${_argc_split_positionals_values[@]}"} ${tails[@]+"${tails[@]}"})"#,
    ),
    (
        r#"=("${_argc_split_positionals_values[@]}")"#,
        r#"=(${_argc_split_positionals_values[@]+"${_argc_split_positionals_values[@]}"})"#,
    ),
];

const ARGC_BUILD_MARKER: &str = "# ARGC-BUILD {";
const ARGC_BUILD_END_MARKER: &str = "# ARGC-BUILD }";

fn tolerate_empty_arrays_under_nounset(build_script: &str) -> String {
    let Some(start) = build_script.find(ARGC_BUILD_MARKER) else {
        return build_script.to_string();
    };
    let end = build_script[start..]
        .find(ARGC_BUILD_END_MARKER)
        .map_or(build_script.len(), |end| {
            start + end + ARGC_BUILD_END_MARKER.len()
        });
    let (before, generated, after) = (
        &build_script[..start],
        &build_script[start..end],
        &build_script[end..],
    );
    let generated = EMPTY_ARRAY_EXPANSIONS
        .iter()
        .fold(generated.to_string(), |script, (needle, replacement)| {
            script.replace(needle, replacement)
        });
    let generated: String = generated
        .split_inclusive('\n')
        .map(nounset_safe_choice_line)
        .collect();
    format!("{before}{generated}{after}")
}

/// Rewrites the trailing `"${NAME[@]}"` of a generated
/// `_argc_validate_choices` line to `${NAME[@]+"${NAME[@]}"}`. Lines that
/// validate a scalar end in `"$NAME"` and are returned unchanged.
fn nounset_safe_choice_line(line: &str) -> Cow<'_, str> {
    let (body, newline) = match line.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (line, ""),
    };
    if !body.trim_start().starts_with("_argc_validate_choices ") {
        return Cow::Borrowed(line);
    }
    let Some((head, name)) = body
        .strip_suffix("[@]}\"")
        .and_then(|rest| rest.rsplit_once("\"${"))
    else {
        return Cow::Borrowed(line);
    };
    let mut chars = name.chars();
    let is_identifier = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !is_identifier {
        return Cow::Borrowed(line);
    }
    Cow::Owned(format!("{head}${{{name}[@]+\"${{{name}[@]}}\"}}{newline}"))
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

    #[cfg(unix)]
    struct TempFiles(Vec<std::path::PathBuf>);

    #[cfg(unix)]
    impl Drop for TempFiles {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }

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

    #[test]
    fn rewrites_argc_empty_array_expansions_to_nounset_safe_form_test() {
        // `forward` is hand-written code that expands the same arrays the
        // rewrite targets, without the generated call around them; a wider
        // rewrite than the listed lines would reach it. Its `arr+=` line is
        // identical to a generated one, so only the block scoping keeps it.
        let forward = "forward() { local arr=() values=(); arr+=(\"${values[@]}\"); printf '%s' \"${_argc_tools[@]}\" \"${argc__positionals[@]}\" \"${arr[@]}\"; }";
        // `split` makes argc emit the delimiter and split-positionals
        // templates; `pick` emits the choice checks for an option, a scalar
        // positional and a multi-value positional.
        let src = format!(
            "# @describe test tool\n# @meta require-tools ls\n# @env NEEDED! An env\n\n# @cmd run\n# @option --content! The content\nrun() {{ :; }}\n\n# @cmd split\n# @option --x*, Delimited values\n# @arg items*, Delimited items\nsplit() {{ :; }}\n\n# @cmd pick\n# @option --y*,[a|b] Delimited choices\n# @arg one[a|b] A choice\n# @arg picks*,[a|b] Delimited choices\npick() {{ :; }}\n\n{forward}\n"
        );
        // A second copy of `forward` after the generated block must survive
        // the rewrite as well.
        let built = format!(
            "{}\n{forward}\n",
            argc::build(&src, "", None).expect("argc build failed")
        );
        let block = built.find(ARGC_BUILD_MARKER).expect("argc build block");
        let end = block
            + built[block..]
                .find(ARGC_BUILD_END_MARKER)
                .expect("argc build block end")
            + ARGC_BUILD_END_MARKER.len();
        let generated = &built[block..end];
        for (needle, _) in EMPTY_ARRAY_EXPANSIONS {
            assert!(
                generated.contains(needle),
                "argc changed its generated `{needle}`; update EMPTY_ARRAY_EXPANSIONS"
            );
        }
        let scalar_choice =
            r#"_argc_validate_choices '`[ONE]`' "$(printf "%s\n" a b)" "$argc_one""#;
        let array_choice =
            r#"_argc_validate_choices '`[PICKS]...`' "$(printf "%s\n" a b)" "${argc_picks[@]}""#;
        for choice in [scalar_choice, array_choice] {
            assert!(
                generated.contains(choice),
                "argc changed its generated `{choice}`; update nounset_safe_choice_line()"
            );
        }

        let fixed = tolerate_empty_arrays_under_nounset(&built);
        let fixed_end = block
            + fixed[block..]
                .find(ARGC_BUILD_END_MARKER)
                .expect("rewritten build block end")
            + ARGC_BUILD_END_MARKER.len();

        for (needle, replacement) in EMPTY_ARRAY_EXPANSIONS {
            assert!(
                !fixed[block..fixed_end].contains(needle),
                "still present: {needle}"
            );
            assert!(
                fixed[block..fixed_end].contains(replacement),
                "missing: {replacement}"
            );
        }
        assert!(fixed[block..fixed_end].contains(scalar_choice));
        assert!(!fixed[block..fixed_end].contains(array_choice));

        // Nothing outside the generated array expansions changes: the
        // hand-written parts before and after the generated block are
        // byte-identical, and inside the block one line changes per
        // occurrence of a listed expansion or array choice check, each by
        // that expansion's replacement alone.
        assert!(
            built[..block].contains(forward),
            "fixture lost its hand-written line"
        );
        assert!(
            built[end..].contains(forward),
            "fixture lost its hand-written line after the block"
        );
        assert_eq!(
            fixed[..block],
            built[..block],
            "the rewrite touched hand-written code"
        );
        assert_eq!(
            fixed[fixed_end..],
            built[end..],
            "the rewrite touched hand-written code after the generated block"
        );
        assert_eq!(fixed.lines().count(), built.lines().count());
        let occurrences: usize = EMPTY_ARRAY_EXPANSIONS
            .iter()
            .map(|(needle, _)| generated.matches(needle).count())
            .sum();
        let array_choices = generated
            .lines()
            .filter(|line| {
                line.trim_start().starts_with("_argc_validate_choices ")
                    && line.ends_with(r#"[@]}""#)
                    && !EMPTY_ARRAY_EXPANSIONS
                        .iter()
                        .any(|(needle, _)| line.contains(needle))
            })
            .count();
        assert_eq!(array_choices, 1, "expected only the `picks` choice check");
        let changed: Vec<(&str, &str)> = built
            .lines()
            .zip(fixed.lines())
            .filter(|(before, after)| before != after)
            .collect();
        assert_eq!(
            changed.len(),
            occurrences + array_choices,
            "the rewrite changed other lines: {changed:#?}"
        );
        for (before, after) in changed {
            let hits: Vec<_> = EMPTY_ARRAY_EXPANSIONS
                .iter()
                .filter(|(needle, _)| before.contains(needle))
                .collect();
            if hits.is_empty() {
                let (head, name) = before
                    .strip_suffix(r#"[@]}""#)
                    .and_then(|rest| rest.rsplit_once(r#""${"#))
                    .unwrap_or_else(|| panic!("an unlisted line changed: {before}"));
                assert!(
                    head.trim_start().starts_with("_argc_validate_choices "),
                    "an unlisted line changed: {before}"
                );
                assert_eq!(
                    after,
                    format!(r#"{head}${{{name}[@]+"${{{name}[@]}}"}}"#),
                    "a choice check differs by more than its array expansion"
                );
                continue;
            }
            assert_eq!(
                hits.len(),
                1,
                "a changed line matched {} needles: {before}",
                hits.len()
            );
            let (needle, replacement) = hits[0];
            assert_eq!(
                after,
                before.replace(needle, replacement),
                "a changed line differs by more than its listed expansion"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn built_tool_runs_a_command_without_positionals_under_nounset_test() {
        use std::fs;
        use std::process::Command;

        // The first bash on PATH, plus the system /bin/bash (3.2 on macOS)
        // when it is a different program, so a host whose PATH leads with a
        // newer bash still runs the old one.
        let mut shells: Vec<std::path::PathBuf> =
            crate::testing::bash_program().into_iter().collect();
        let system = Path::new("/bin/bash");
        if system.is_file()
            && !shells
                .iter()
                .any(|s| fs::canonicalize(s).ok() == fs::canonicalize(system).ok())
        {
            shells.push(system.to_path_buf());
        }
        if shells.is_empty() {
            eprintln!("skipping: bash not available");
            return;
        }

        // `strict` declares a required option, a required env and a required
        // tool, so the generated `_argc_require_params` and
        // `_argc_require_tools` calls exist and run, with empty arrays, on a
        // `greet` call that passes no options. `split` takes the delimited
        // option and positional paths, where an empty value or an empty item
        // leaves the split arrays empty; `pick` takes the same paths through
        // the choice checks.
        let src = "#!/usr/bin/env bash\nset -euo pipefail\n# @describe t\n\n# @cmd greet\n# @option --name The name\ngreet() { printf 'hi %s' \"${argc_name:-nobody}\" >> \"$LLM_OUTPUT\"; }\n\n# @cmd strict\n# @meta require-tools sh\n# @option --x! A required option\n# @env NEEDED! A required env\nstrict() { :; }\n\n# @cmd split\n# @option --x*, Delimited values\n# @arg items*, Delimited items\nsplit() { printf 'x=%s items=%s' \"${argc_x[*]:-}\" \"${argc_items[*]:-}\" >> \"$LLM_OUTPUT\"; }\n\n# @cmd pick\n# @option --y*,[a|b] Delimited choices\n# @arg picks*,[a|b] Delimited choices\npick() { printf 'y=%s picks=%s' \"${argc_y[*]:-}\" \"${argc_picks[*]:-}\" >> \"$LLM_OUTPUT\"; }\n";
        let path = crate::utils::temp_file("bash-nounset", ".sh");
        let mut temps = TempFiles(vec![path.clone()]);
        fs::write(&path, src).expect("failed to write temp script");
        generate_bash_declarations(File::open(&path).unwrap(), &path, "t").expect("build failed");
        let built = fs::read_to_string(&path).unwrap();
        for generated in [
            "_argc_require_params",
            "_argc_require_tools",
            "_argc_split_positionals",
            "_argc_validate_choices",
        ] {
            assert!(
                built.contains(generated),
                "the fixture no longer exercises {generated}"
            );
        }
        // Only `strict` requires `sh`, so its tool list must be filled in
        // its own parse function and nowhere else.
        let strict_tools = "_argc_tools=(sh)";
        let strict_start = built
            .find("_argc_parse_strict() {\n")
            .expect("argc parse function for `strict`");
        let strict_len = built[strict_start..]
            .find("\n}\n")
            .expect("end of the `strict` parse function");
        let strict_body = &built[strict_start..strict_start + strict_len];
        assert_eq!(
            strict_body.matches(strict_tools).count(),
            built.matches(strict_tools).count(),
            "`{strict_tools}` appears outside `_argc_parse_strict`"
        );
        assert_eq!(strict_body.matches(strict_tools).count(), 1);

        let cases: [(&[&str], &str); 8] = [
            (&["greet"], "hi nobody"),
            (&["split", "--x="], "x= items="),
            (&["split", "--x=a,b", "c,d"], "x=a b items=c d"),
            (&["split", "--x=a,,b", "c", "", "d"], "x=a  b items=c d"),
            (&["split", ""], "x= items="),
            (&["pick", "--y="], "y= picks="),
            (&["pick", ""], "y= picks="),
            (&["pick", "--y=b,a", "a,b"], "y=b a picks=a b"),
        ];
        for bash in &shells {
            for (args, expected) in cases {
                let out = crate::utils::temp_file("bash-nounset-out", ".txt");
                temps.0.push(out.clone());
                let output = Command::new(bash)
                    .arg(&path)
                    .args(args)
                    .env("LLM_OUTPUT", &out)
                    .env_remove("NEEDED")
                    .output()
                    .expect("failed to run built script");
                assert!(
                    output.status.success(),
                    "built script failed for {args:?} under {}: {}",
                    bash.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    fs::read_to_string(&out).unwrap(),
                    expected,
                    "{args:?} under {}",
                    bash.display()
                );
            }
        }
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
        let mut temps = TempFiles(vec![path.clone()]);
        fs::write(&path, src).expect("failed to write temp script");
        generate_bash_declarations(File::open(&path).unwrap(), &path, "t").expect("build failed");

        let mut run = |args: &[&str]| -> String {
            let out = crate::utils::temp_file("bash-attached-out", ".txt");
            temps.0.push(out.clone());
            let status = Command::new("bash")
                .arg(&path)
                .args(args)
                .env("LLM_OUTPUT", &out)
                .status()
                .expect("failed to run built script");
            assert!(status.success(), "built script failed for {args:?}");
            fs::read_to_string(&out).unwrap()
        };

        assert_eq!(run(&["--v=a=b=c"]), "a=b=c");
        assert_eq!(run(&["--v", "a=b=c"]), "a=b=c");
        assert_eq!(run(&["--v=plain"]), "plain");
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
