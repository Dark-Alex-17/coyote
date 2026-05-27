use crate::function::FunctionDeclaration;
use crate::parsers::common::{self, Param, ScriptedLanguage};
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use tree_sitter::Node;

pub(crate) struct TypeScriptLanguage;

impl ScriptedLanguage for TypeScriptLanguage {
    fn ts_language(&self) -> tree_sitter::Language {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }

    fn lang_name(&self) -> &str {
        "typescript"
    }

    fn find_functions<'a>(&self, root: Node<'a>, _src: &str) -> Vec<(Node<'a>, Node<'a>)> {
        let mut cursor = root.walk();
        root.named_children(&mut cursor)
            .filter_map(|stmt| match stmt.kind() {
                "export_statement" => unwrap_exported_function(stmt).map(|fd| (stmt, fd)),
                _ => None,
            })
            .collect()
    }

    fn function_name<'a>(&self, func_node: Node<'a>, src: &'a str) -> Result<&'a str> {
        let name_node = func_node
            .child_by_field_name("name")
            .ok_or_else(|| anyhow!("function_declaration missing name"))?;
        common::node_text(name_node, src)
    }

    fn extract_description(
        &self,
        wrapper_node: Node<'_>,
        func_node: Node<'_>,
        src: &str,
    ) -> Option<String> {
        let text = jsdoc_text(wrapper_node, func_node, src)?;
        let lines = clean_jsdoc_lines(text);
        let mut description = Vec::new();
        for line in lines {
            if line.starts_with('@') {
                break;
            }
            description.push(line);
        }

        let description = description.join("\n").trim().to_string();
        (!description.is_empty()).then_some(description)
    }

    fn extract_params(
        &self,
        func_node: Node<'_>,
        src: &str,
        _description: &str,
    ) -> Result<Vec<Param>> {
        let parameters = func_node
            .child_by_field_name("parameters")
            .ok_or_else(|| anyhow!("function_declaration missing parameters"))?;
        let mut out = Vec::new();
        let mut cursor = parameters.walk();

        for param in parameters.named_children(&mut cursor) {
            match param.kind() {
                "required_parameter" | "optional_parameter" => {
                    let name = parameter_name(param, src)?;
                    let ty = get_arg_type(param.child_by_field_name("type"), src)?;
                    let required = param.kind() == "required_parameter"
                        && param.child_by_field_name("value").is_none();
                    let default = param.child_by_field_name("value").map(|_| Value::Null);
                    out.push(common::build_param(name, ty, required, default));
                }
                "rest_parameter" => {
                    let line = param.start_position().row + 1;
                    bail!("line {line}: rest parameters (...) are not supported in tool functions")
                }
                "object_pattern" => {
                    let line = param.start_position().row + 1;
                    bail!(
                        "line {line}: destructured object parameters (e.g. '{{ a, b }}: {{ a: string }}') \
                         are not supported in tool functions. Use flat parameters instead (e.g. 'a: string, b: string')."
                    )
                }
                other => {
                    let line = param.start_position().row + 1;
                    bail!("line {line}: unsupported parameter type: {other}")
                }
            }
        }

        let wrapper = match func_node.parent() {
            Some(parent) if parent.kind() == "export_statement" => parent,
            _ => func_node,
        };
        if let Some(doc) = jsdoc_text(wrapper, func_node, src) {
            let meta = parse_jsdoc_params(doc);
            for p in &mut out {
                if let Some(desc) = meta.get(&p.name)
                    && !desc.is_empty()
                {
                    p.doc_desc = Some(desc.clone());
                }
            }
        }

        Ok(out)
    }
}

pub fn generate_typescript_declarations(
    mut tool_file: File,
    file_name: &str,
    parent: Option<&Path>,
) -> Result<Vec<FunctionDeclaration>> {
    let mut src = String::new();
    tool_file
        .read_to_string(&mut src)
        .with_context(|| format!("Failed to load script at '{tool_file:?}'"))?;

    let is_tool = parent
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "tools");

    common::generate_declarations(&TypeScriptLanguage, &src, file_name, is_tool)
}

fn unwrap_exported_function(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("declaration")
        .filter(|child| child.kind() == "function_declaration")
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "function_declaration")
        })
}

fn jsdoc_text<'a>(wrapper_node: Node<'_>, func_node: Node<'_>, src: &'a str) -> Option<&'a str> {
    wrapper_node
        .prev_named_sibling()
        .or_else(|| func_node.prev_named_sibling())
        .filter(|node| node.kind() == "comment")
        .and_then(|node| common::node_text(node, src).ok())
        .filter(|text| text.trim_start().starts_with("/**"))
}

fn clean_jsdoc_lines(doc: &str) -> Vec<String> {
    let trimmed = doc.trim();
    let inner = trimmed
        .strip_prefix("/**")
        .unwrap_or(trimmed)
        .strip_suffix("*/")
        .unwrap_or(trimmed);

    inner
        .lines()
        .map(|line| {
            let line = line.trim();
            let line = line.strip_prefix('*').unwrap_or(line).trim_start();
            line.to_string()
        })
        .collect()
}

fn parse_jsdoc_params(doc: &str) -> IndexMap<String, String> {
    let mut out = IndexMap::new();

    for line in clean_jsdoc_lines(doc) {
        let Some(rest) = line.strip_prefix("@param") else {
            continue;
        };

        let mut rest = rest.trim();
        if rest.starts_with('{')
            && let Some(end) = rest.find('}')
        {
            rest = rest[end + 1..].trim_start();
        }

        if rest.is_empty() {
            continue;
        }

        let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let mut name = rest[..name_end].trim();
        if let Some(stripped) = name.strip_suffix('?') {
            name = stripped;
        }

        if name.is_empty() {
            continue;
        }

        let mut desc = rest[name_end..].trim();
        if let Some(stripped) = desc.strip_prefix('-') {
            desc = stripped.trim_start();
        }

        out.insert(name.to_string(), desc.to_string());
    }

    out
}

fn parameter_name<'a>(node: Node<'_>, src: &'a str) -> Result<&'a str> {
    if let Some(name) = node.child_by_field_name("name") {
        return match name.kind() {
            "identifier" => common::node_text(name, src),
            "rest_pattern" => {
                let line = node.start_position().row + 1;
                bail!("line {line}: rest parameters (...) are not supported in tool functions")
            }
            "object_pattern" | "array_pattern" => {
                let line = node.start_position().row + 1;
                bail!(
                    "line {line}: destructured parameters are not supported in tool functions. \
                     Use flat parameters instead (e.g. 'a: string, b: string')."
                )
            }
            other => {
                let line = node.start_position().row + 1;
                bail!("line {line}: unsupported parameter type: {other}")
            }
        };
    }

    let pattern = node
        .child_by_field_name("pattern")
        .ok_or_else(|| anyhow!("parameter missing pattern"))?;

    match pattern.kind() {
        "identifier" => common::node_text(pattern, src),
        "rest_pattern" => {
            let line = node.start_position().row + 1;
            bail!("line {line}: rest parameters (...) are not supported in tool functions")
        }
        "object_pattern" | "array_pattern" => {
            let line = node.start_position().row + 1;
            bail!(
                "line {line}: destructured parameters are not supported in tool functions. \
                 Use flat parameters instead (e.g. 'a: string, b: string')."
            )
        }
        other => {
            let line = node.start_position().row + 1;
            bail!("line {line}: unsupported parameter type: {other}")
        }
    }
}

fn get_arg_type(annotation: Option<Node<'_>>, src: &str) -> Result<String> {
    let Some(annotation) = annotation else {
        return Ok(String::new());
    };

    match annotation.kind() {
        "type_annotation" | "type" => get_arg_type(common::named_child(annotation, 0), src),
        "predefined_type" => Ok(match common::node_text(annotation, src)? {
            "string" => "str",
            "number" => "float",
            "boolean" => "bool",
            "any" | "unknown" | "void" | "undefined" => "any",
            _ => "any",
        }
        .to_string()),
        "type_identifier" | "nested_type_identifier" => Ok("any".to_string()),
        "generic_type" => {
            let name = annotation
                .child_by_field_name("name")
                .ok_or_else(|| anyhow!("generic_type missing name"))?;
            let type_name = common::node_text(name, src)?;
            let type_args = annotation
                .child_by_field_name("type_arguments")
                .ok_or_else(|| anyhow!("generic_type missing type arguments"))?;
            let inner = common::named_child(type_args, 0)
                .ok_or_else(|| anyhow!("generic_type missing inner type"))?;

            match type_name {
                "Array" => Ok(format!("list[{}]", get_arg_type(Some(inner), src)?)),
                _ => Ok("any".to_string()),
            }
        }
        "array_type" => {
            let inner = common::named_child(annotation, 0)
                .ok_or_else(|| anyhow!("array_type missing inner type"))?;
            Ok(format!("list[{}]", get_arg_type(Some(inner), src)?))
        }
        "union_type" => resolve_union_type(annotation, src),
        "literal_type" => resolve_literal_type(annotation, src),
        "parenthesized_type" => get_arg_type(common::named_child(annotation, 0), src),
        _ => Ok("any".to_string()),
    }
}

fn resolve_union_type(annotation: Node<'_>, src: &str) -> Result<String> {
    let members = flatten_union_members(annotation);
    let has_null = members.iter().any(|member| is_nullish_type(*member, src));

    let mut literal_values = Vec::new();
    let mut all_string_literals = true;
    for member in &members {
        match string_literal_member(*member, src) {
            Some(value) => literal_values.push(value),
            None => {
                all_string_literals = false;
                break;
            }
        }
    }

    if all_string_literals && !literal_values.is_empty() {
        return Ok(format!("literal:{}", literal_values.join("|")));
    }

    let mut first_non_null = None;
    for member in members {
        if is_nullish_type(member, src) {
            continue;
        }
        first_non_null = Some(get_arg_type(Some(member), src)?);
        break;
    }

    let mut ty = first_non_null.unwrap_or_else(|| "any".to_string());
    if has_null && !ty.ends_with('?') {
        ty.push('?');
    }
    Ok(ty)
}

fn flatten_union_members(node: Node<'_>) -> Vec<Node<'_>> {
    let node = if node.kind() == "type" {
        match common::named_child(node, 0) {
            Some(inner) => inner,
            None => return vec![],
        }
    } else {
        node
    };

    if node.kind() != "union_type" {
        return vec![node];
    }

    let mut cursor = node.walk();
    let mut out = Vec::new();
    for child in node.named_children(&mut cursor) {
        out.extend(flatten_union_members(child));
    }
    out
}

fn resolve_literal_type(annotation: Node<'_>, src: &str) -> Result<String> {
    let inner = common::named_child(annotation, 0)
        .ok_or_else(|| anyhow!("literal_type missing inner literal"))?;

    match inner.kind() {
        "string" | "number" | "true" | "false" | "unary_expression" => {
            Ok(format!("literal:{}", common::node_text(inner, src)?.trim()))
        }
        "null" | "undefined" => Ok("any".to_string()),
        _ => Ok("any".to_string()),
    }
}

fn string_literal_member(node: Node<'_>, src: &str) -> Option<String> {
    let node = if node.kind() == "type" {
        common::named_child(node, 0)?
    } else {
        node
    };

    if node.kind() != "literal_type" {
        return None;
    }

    let inner = common::named_child(node, 0)?;
    if inner.kind() != "string" {
        return None;
    }

    Some(common::node_text(inner, src).ok()?.to_string())
}

fn is_nullish_type(node: Node<'_>, src: &str) -> bool {
    let node = if node.kind() == "type" {
        match common::named_child(node, 0) {
            Some(inner) => inner,
            None => return false,
        }
    } else {
        node
    };

    match node.kind() {
        "literal_type" => common::named_child(node, 0)
            .is_some_and(|inner| matches!(inner.kind(), "null" | "undefined")),
        "predefined_type" => common::node_text(node, src)
            .map(|text| matches!(text, "undefined" | "void"))
            .unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::JsonSchema;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parse_ts_source(
        source: &str,
        file_name: &str,
        parent: &Path,
    ) -> Result<Vec<FunctionDeclaration>> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("coyote_ts_parser_{file_name}_{unique}.ts"));
        fs::write(&path, source).expect("write");
        let file = File::open(&path).expect("open");
        let result = generate_typescript_declarations(file, file_name, Some(parent));
        let _ = fs::remove_file(&path);
        result
    }

    fn properties(schema: &JsonSchema) -> &IndexMap<String, JsonSchema> {
        schema
            .properties
            .as_ref()
            .expect("missing schema properties")
    }

    fn property<'a>(schema: &'a JsonSchema, name: &str) -> &'a JsonSchema {
        properties(schema)
            .get(name)
            .unwrap_or_else(|| panic!("missing property: {name}"))
    }

    #[test]
    fn test_ts_tool_demo() {
        let source = r#"
/**
 * Demonstrates how to create a tool using TypeScript.
 *
 * @param query - The search query string
 * @param format - Output format
 * @param count - Maximum results to return
 * @param verbose - Enable verbose output
 * @param tags - List of tags to filter by
 * @param language - Optional language filter
 * @param extra_tags - Optional extra tags
 */
export function run(
  query: string,
  format: "json" | "csv" | "xml",
  count: number,
  verbose: boolean,
  tags: string[],
  language?: string,
  extra_tags?: Array<string>,
): string {
  return "result";
}
"#;

        let declarations = parse_ts_source(source, "demo_ts", Path::new("tools")).unwrap();
        assert_eq!(declarations.len(), 1);

        let decl = &declarations[0];
        assert_eq!(decl.name, "demo_ts");
        assert!(!decl.agent);

        let params = &decl.parameters;
        assert_eq!(params.type_value.as_deref(), Some("object"));
        assert_eq!(
            params.required.as_ref().unwrap(),
            &vec![
                "query".to_string(),
                "format".to_string(),
                "count".to_string(),
                "verbose".to_string(),
                "tags".to_string(),
            ]
        );

        assert_eq!(
            property(params, "query").type_value.as_deref(),
            Some("string")
        );

        let format = property(params, "format");
        assert_eq!(format.type_value.as_deref(), Some("string"));
        assert_eq!(
            format.enum_value.as_ref().unwrap(),
            &vec!["json".to_string(), "csv".to_string(), "xml".to_string()]
        );

        assert_eq!(
            property(params, "count").type_value.as_deref(),
            Some("number")
        );
        assert_eq!(
            property(params, "verbose").type_value.as_deref(),
            Some("boolean")
        );

        let tags = property(params, "tags");
        assert_eq!(tags.type_value.as_deref(), Some("array"));
        assert_eq!(
            tags.items.as_ref().unwrap().type_value.as_deref(),
            Some("string")
        );

        let language = property(params, "language");
        assert_eq!(language.type_value.as_deref(), Some("string"));
        assert!(
            !params
                .required
                .as_ref()
                .unwrap()
                .contains(&"language".to_string())
        );

        let extra_tags = property(params, "extra_tags");
        assert_eq!(extra_tags.type_value.as_deref(), Some("array"));
        assert_eq!(
            extra_tags.items.as_ref().unwrap().type_value.as_deref(),
            Some("string")
        );
        assert!(
            !params
                .required
                .as_ref()
                .unwrap()
                .contains(&"extra_tags".to_string())
        );
    }

    #[test]
    fn test_ts_tool_simple() {
        let source = r#"
/**
 * Execute the given code.
 *
 * @param code - The code to execute
 */
export function run(code: string): string {
  return eval(code);
}
"#;

        let declarations = parse_ts_source(source, "execute_code", Path::new("tools")).unwrap();
        assert_eq!(declarations.len(), 1);

        let decl = &declarations[0];
        assert_eq!(decl.name, "execute_code");
        assert!(!decl.agent);

        let params = &decl.parameters;
        assert_eq!(params.required.as_ref().unwrap(), &vec!["code".to_string()]);
        assert_eq!(
            property(params, "code").type_value.as_deref(),
            Some("string")
        );
    }

    #[test]
    fn test_ts_agent_tools() {
        let source = r#"
/** Get user info by ID */
export function get_user(id: string): string {
  return "";
}

/** List all users */
export function list_users(): string {
  return "";
}
"#;

        let declarations = parse_ts_source(source, "tools", Path::new("demo")).unwrap();
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0].name, "get_user");
        assert_eq!(declarations[1].name, "list_users");
        assert!(declarations[0].agent);
        assert!(declarations[1].agent);
    }

    #[test]
    fn test_ts_reject_rest_params() {
        let source = r#"
/**
 * Has rest params
 */
export function run(...args: string[]): string {
  return "";
}
"#;

        let err = parse_ts_source(source, "rest_params", Path::new("tools")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("rest parameters"));
        assert!(msg.contains("in function 'run'"));
    }

    #[test]
    fn test_ts_missing_jsdoc() {
        let source = r#"
export function run(x: string): string {
  return x;
}
"#;

        let err = parse_ts_source(source, "missing_jsdoc", Path::new("tools")).unwrap_err();
        assert!(
            err.to_string()
                .contains("Missing or empty description on function: run")
        );
    }

    #[test]
    fn test_ts_syntax_error() {
        let source = "export function run(: broken";
        let err = parse_ts_source(source, "syntax_error", Path::new("tools")).unwrap_err();
        assert!(err.to_string().contains("failed to parse typescript"));
    }

    #[test]
    fn test_ts_underscore_skipped() {
        let source = r#"
/** Private helper */
function _helper(): void {}

/** Public function */
export function do_stuff(): string {
  return "";
}
"#;

        let declarations = parse_ts_source(source, "tools", Path::new("demo")).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "do_stuff");
        assert!(declarations[0].agent);
    }

    #[test]
    fn test_ts_non_exported_helpers_skipped() {
        let source = r#"
#!/usr/bin/env tsx

import { appendFileSync } from 'fs';

/**
 * Get the current weather in a given location
 * @param location - The city
 */
export function get_current_weather(location: string): string {
  return fetchSync("https://example.com/" + location);
}

function fetchSync(url: string): string {
  return "sunny";
}
"#;

        let declarations = parse_ts_source(source, "tools", Path::new("demo")).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "get_current_weather");
    }

    #[test]
    fn test_ts_instructions_not_skipped() {
        let source = r#"
/** Help text for the agent */
export function _instructions(): string {
  return "";
}
"#;

        let declarations = parse_ts_source(source, "tools", Path::new("demo")).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "instructions");
        assert!(declarations[0].agent);
    }

    #[test]
    fn test_ts_optional_with_null_union() {
        let source = r#"
/**
 * Fetch data with optional filter
 *
 * @param url - The URL to fetch
 * @param filter - Optional filter string
 */
export function run(url: string, filter: string | null): string {
  return "";
}
"#;

        let declarations = parse_ts_source(source, "fetch_data", Path::new("tools")).unwrap();
        let params = &declarations[0].parameters;
        assert!(
            params
                .required
                .as_ref()
                .unwrap()
                .contains(&"url".to_string())
        );
        assert!(
            !params
                .required
                .as_ref()
                .unwrap()
                .contains(&"filter".to_string())
        );
        assert_eq!(
            property(params, "filter").type_value.as_deref(),
            Some("string")
        );
    }

    #[test]
    fn test_ts_optional_with_default() {
        let source = r#"
/**
 * Search with limit
 *
 * @param query - Search query
 * @param limit - Max results
 */
export function run(query: string, limit: number = 10): string {
  return "";
}
"#;

        let declarations =
            parse_ts_source(source, "search_with_limit", Path::new("tools")).unwrap();
        let params = &declarations[0].parameters;
        assert!(
            params
                .required
                .as_ref()
                .unwrap()
                .contains(&"query".to_string())
        );
        assert!(
            !params
                .required
                .as_ref()
                .unwrap()
                .contains(&"limit".to_string())
        );
        assert_eq!(
            property(params, "limit").type_value.as_deref(),
            Some("number")
        );
    }

    #[test]
    fn test_ts_shebang_parses() {
        let source = r#"#!/usr/bin/env tsx

/**
 * Get weather
 * @param location - The city
 */
export function run(location: string): string {
  return location;
}
"#;

        let result = parse_ts_source(source, "get_weather", Path::new("tools"));
        eprintln!("shebang parse result: {result:?}");
        assert!(result.is_ok(), "shebang should not cause parse failure");
        let declarations = result.unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "get_weather");
    }
}
