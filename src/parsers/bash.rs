use crate::function::{FunctionDeclaration, JsonSchema};
use anyhow::{Context, Result, bail};
use argc::{ChoiceValue, CommandValue, FlagOptionValue};
use indexmap::IndexMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::{env, fs};

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
    let build_script = argc::build(
        &src,
        "",
        env::var("TERM_WIDTH").ok().and_then(|v| v.parse().ok()),
    )?;
    fs::write(tools_file_path, &build_script)
        .with_context(|| format!("Failed to write built script to '{tools_file_path:?}'"))?;

    let command_value = argc::export(&build_script, file_name)
        .with_context(|| format!("Failed to parse script at '{tool_file:?}'"))?;
    if command_value.subcommands.is_empty() {
        let function_declaration =
            command_to_function_declaration(&command_value).ok_or_else(|| {
                anyhow::format_err!("Tool definition missing or empty description: {file_name}")
            })?;
        Ok(vec![function_declaration])
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

        Ok(declarations)
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
