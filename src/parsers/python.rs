use crate::function::FunctionDeclaration;
use crate::parsers::common::{self, Param, ScriptedLanguage};
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use tree_sitter::Node;

pub(crate) struct PythonLanguage;

impl ScriptedLanguage for PythonLanguage {
    fn ts_language(&self) -> tree_sitter::Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn lang_name(&self) -> &str {
        "python"
    }

    fn find_functions<'a>(&self, root: Node<'a>, _src: &str) -> Vec<(Node<'a>, Node<'a>)> {
        let mut cursor = root.walk();
        root.named_children(&mut cursor)
            .filter_map(|stmt| unwrap_function_definition(stmt).map(|fd| (stmt, fd)))
            .collect()
    }

    fn function_name<'a>(&self, func_node: Node<'a>, src: &'a str) -> Result<&'a str> {
        let name_node = func_node
            .child_by_field_name("name")
            .ok_or_else(|| anyhow!("function_definition missing name"))?;
        common::node_text(name_node, src)
    }

    fn extract_description(
        &self,
        _wrapper_node: Node<'_>,
        func_node: Node<'_>,
        src: &str,
    ) -> Option<String> {
        get_docstring_from_function(func_node, src)
    }

    fn extract_params(
        &self,
        func_node: Node<'_>,
        src: &str,
        description: &str,
    ) -> Result<Vec<Param>> {
        let parameters = func_node
            .child_by_field_name("parameters")
            .ok_or_else(|| anyhow!("function_definition missing parameters"))?;
        let mut out = Vec::new();
        let mut cursor = parameters.walk();

        for param in parameters.named_children(&mut cursor) {
            match param.kind() {
                "identifier" => out.push(Param {
                    name: common::node_text(param, src)?.to_string(),
                    ty_hint: String::new(),
                    required: true,
                    default: None,
                    doc_type: None,
                    doc_desc: None,
                }),
                "typed_parameter" => out.push(common::build_param(
                    parameter_name(param, src)?,
                    get_arg_type(param.child_by_field_name("type"), src)?,
                    true,
                    None,
                )),
                "default_parameter" => out.push(common::build_param(
                    parameter_name(param, src)?,
                    String::new(),
                    false,
                    Some(Value::Null),
                )),
                "typed_default_parameter" => out.push(common::build_param(
                    parameter_name(param, src)?,
                    get_arg_type(param.child_by_field_name("type"), src)?,
                    false,
                    Some(Value::Null),
                )),
                "list_splat_pattern" | "dictionary_splat_pattern" | "positional_separator" => {
                    let line = param.start_position().row + 1;
                    bail!(
                        "line {line}: *args/*kwargs/positional-only parameters are not supported in tool functions"
                    )
                }
                "keyword_separator" => continue,
                other => {
                    let line = param.start_position().row + 1;
                    bail!("line {line}: unsupported parameter type: {other}")
                }
            }
        }

        let meta = parse_docstring_args(description);
        for p in &mut out {
            if let Some((t, d)) = meta.get(&p.name) {
                if !t.is_empty() {
                    p.doc_type = Some(t.clone());
                }

                if !d.is_empty() {
                    p.doc_desc = Some(d.clone());
                }

                if t.ends_with('?') {
                    p.required = false;
                }
            }
        }

        Ok(out)
    }
}

pub fn generate_python_declarations(
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

    common::generate_declarations(&PythonLanguage, &src, file_name, is_tool)
}

fn unwrap_function_definition(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "function_definition" => Some(node),
        "decorated_definition" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "function_definition")
        }
        _ => None,
    }
}

fn get_docstring_from_function(node: Node<'_>, src: &str) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    if first.kind() != "expression_statement" {
        return None;
    }

    let mut expr_cursor = first.walk();
    let expr = first.named_children(&mut expr_cursor).next()?;
    if expr.kind() != "string" {
        return None;
    }

    let text = common::node_text(expr, src).ok()?;
    strip_string_quotes(text)
}

fn strip_string_quotes(text: &str) -> Option<String> {
    let quote_offset = text
        .char_indices()
        .find_map(|(idx, ch)| (ch == '\'' || ch == '"').then_some(idx))?;
    let prefix = &text[..quote_offset];
    if !prefix.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return None;
    }
    if prefix.chars().any(|ch| ch == 'f' || ch == 'F') {
        return None;
    }

    let literal = &text[quote_offset..];
    let quote = if literal.starts_with("\"\"\"") {
        "\"\"\""
    } else if literal.starts_with("'''") {
        "'''"
    } else if literal.starts_with('"') {
        "\""
    } else if literal.starts_with('\'') {
        "'"
    } else {
        return None;
    };

    if literal.len() < quote.len() * 2 || !literal.ends_with(quote) {
        return None;
    }

    Some(literal[quote.len()..literal.len() - quote.len()].to_string())
}

fn parameter_name<'a>(node: Node<'_>, src: &'a str) -> Result<&'a str> {
    if let Some(name) = node.child_by_field_name("name") {
        return common::node_text(name, src);
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "identifier")
        .ok_or_else(|| anyhow!("parameter missing name"))
        .and_then(|name| common::node_text(name, src))
}

fn get_arg_type(annotation: Option<Node<'_>>, src: &str) -> Result<String> {
    let Some(annotation) = annotation else {
        return Ok(String::new());
    };

    match annotation.kind() {
        "type" => get_arg_type(common::named_child(annotation, 0), src),
        "generic_type" => {
            let value = annotation
                .child_by_field_name("type")
                .or_else(|| common::named_child(annotation, 0))
                .ok_or_else(|| anyhow!("generic_type missing value"))?;
            let value_name = if value.kind() == "identifier" {
                common::node_text(value, src)?
            } else {
                return Ok("any".to_string());
            };

            let inner = annotation
                .child_by_field_name("type_parameter")
                .or_else(|| annotation.child_by_field_name("parameters"))
                .or_else(|| common::named_child(annotation, 1))
                .ok_or_else(|| anyhow!("generic_type missing inner type"))?;

            match value_name {
                "Optional" => Ok(format!("{}?", generic_inner_type(inner, src)?)),
                "List" => Ok(format!("list[{}]", generic_inner_type(inner, src)?)),
                "Literal" => Ok(format!(
                    "literal:{}",
                    literal_members(inner, src)?.join("|")
                )),
                _ => Ok("any".to_string()),
            }
        }
        "identifier" => Ok(common::node_text(annotation, src)?.to_string()),
        "subscript" => {
            let value = annotation
                .child_by_field_name("value")
                .or_else(|| common::named_child(annotation, 0))
                .ok_or_else(|| anyhow!("subscript missing value"))?;
            let value_name = if value.kind() == "identifier" {
                common::node_text(value, src)?
            } else {
                return Ok("any".to_string());
            };

            let inner = annotation
                .child_by_field_name("subscript")
                .or_else(|| annotation.child_by_field_name("slice"))
                .or_else(|| common::named_child(annotation, 1))
                .ok_or_else(|| anyhow!("subscript missing inner type"))?;
            match value_name {
                "Optional" => Ok(format!("{}?", get_arg_type(Some(inner), src)?)),
                "List" => Ok(format!("list[{}]", get_arg_type(Some(inner), src)?)),
                "Literal" => Ok(format!(
                    "literal:{}",
                    literal_members(inner, src)?.join("|")
                )),
                _ => Ok("any".to_string()),
            }
        }
        _ => Ok("any".to_string()),
    }
}

fn generic_inner_type(node: Node<'_>, src: &str) -> Result<String> {
    if node.kind() == "type_parameter" {
        return get_arg_type(common::named_child(node, 0), src);
    }

    get_arg_type(Some(node), src)
}

fn literal_members(node: Node<'_>, src: &str) -> Result<Vec<String>> {
    if node.kind() == "type" {
        return literal_members(
            common::named_child(node, 0).ok_or_else(|| anyhow!("type missing inner literal"))?,
            src,
        );
    }

    if node.kind() == "tuple" || node.kind() == "type_parameter" {
        let mut cursor = node.walk();
        let members = node
            .named_children(&mut cursor)
            .map(|child| expr_to_str(child, src))
            .collect::<Result<Vec<_>>>()?;

        return Ok(if members.is_empty() {
            vec!["any".to_string()]
        } else {
            members
        });
    }

    Ok(vec![expr_to_str(node, src)?])
}

fn expr_to_str(node: Node<'_>, src: &str) -> Result<String> {
    match node.kind() {
        "type" => expr_to_str(
            common::named_child(node, 0).ok_or_else(|| anyhow!("type missing expression"))?,
            src,
        ),
        "string" | "integer" | "float" | "true" | "false" | "none" | "identifier"
        | "unary_operator" => Ok(common::node_text(node, src)?.trim().to_string()),
        _ => Ok("any".to_string()),
    }
}

fn parse_docstring_args(doc: &str) -> IndexMap<String, (String, String)> {
    let mut out = IndexMap::new();
    let mut in_args = false;
    for line in doc.lines() {
        if !in_args {
            if line.trim_start().starts_with("Args:") {
                in_args = true;
            }
            continue;
        }
        if !(line.starts_with(' ') || line.starts_with('\t')) {
            break;
        }
        let s = line.trim();
        if let Some((left, desc)) = s.split_once(':') {
            let left = left.trim();
            let mut name = left.to_string();
            let mut ty = String::new();
            if let Some((n, t)) = left.split_once(' ') {
                name = n.trim().to_string();
                ty = t.trim().to_string();
                if ty.starts_with('(') && ty.ends_with(')') {
                    let mut inner = ty[1..ty.len() - 1].to_string();
                    if inner.to_lowercase().contains("optional") && !inner.ends_with('?') {
                        inner.push('?');
                    }
                    ty = inner;
                }
            }
            out.insert(name, (ty, desc.trim().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::JsonSchema;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parse_source(
        source: &str,
        file_name: &str,
        parent: &Path,
    ) -> Result<Vec<FunctionDeclaration>> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("loki_python_parser_{file_name}_{unique}.py"));
        fs::write(&path, source).expect("failed to write temp python source");
        let file = File::open(&path).expect("failed to open temp python source");
        let result = generate_python_declarations(file, file_name, Some(parent));
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
    fn test_tool_demo_py() {
        let source = r#"
import os
from typing import List, Literal, Optional

def run(
    string: str,
    string_enum: Literal["foo", "bar"],
    boolean: bool,
    integer: int,
    number: float,
    array: List[str],
    string_optional: Optional[str] = None,
    array_optional: Optional[List[str]] = None,
):
    """Demonstrates how to create a tool using Python and how to use comments.
    Args:
        string: Define a required string property
        string_enum: Define a required string property with enum
        boolean: Define a required boolean property
        integer: Define a required integer property
        number: Define a required number property
        array: Define a required string array property
        string_optional: Define an optional string property
        array_optional: Define an optional string array property
    """
    output = f"""string: {string}
string_enum: {string_enum}
string_optional: {string_optional}
boolean: {boolean}
integer: {integer}
number: {number}
array: {array}
array_optional: {array_optional}"""

    for key, value in os.environ.items():
        if key.startswith("LLM_"):
            output = f"{output}\n{key}: {value}"

    return output
"#;

        let declarations = parse_source(source, "demo_py", Path::new("tools")).unwrap();
        assert_eq!(declarations.len(), 1);

        let decl = &declarations[0];
        assert_eq!(decl.name, "demo_py");
        assert!(!decl.agent);
        assert!(decl.description.starts_with("Demonstrates how to create"));

        let params = &decl.parameters;
        assert_eq!(params.type_value.as_deref(), Some("object"));
        assert_eq!(
            params.required.as_ref().unwrap(),
            &vec![
                "string".to_string(),
                "string_enum".to_string(),
                "boolean".to_string(),
                "integer".to_string(),
                "number".to_string(),
                "array".to_string(),
            ]
        );

        assert_eq!(
            property(params, "string").type_value.as_deref(),
            Some("string")
        );

        let string_enum = property(params, "string_enum");
        assert_eq!(string_enum.type_value.as_deref(), Some("string"));
        assert_eq!(
            string_enum.enum_value.as_ref().unwrap(),
            &vec!["foo".to_string(), "bar".to_string()]
        );

        assert_eq!(
            property(params, "boolean").type_value.as_deref(),
            Some("boolean")
        );
        assert_eq!(
            property(params, "integer").type_value.as_deref(),
            Some("integer")
        );
        assert_eq!(
            property(params, "number").type_value.as_deref(),
            Some("number")
        );

        let array = property(params, "array");
        assert_eq!(array.type_value.as_deref(), Some("array"));
        assert_eq!(
            array.items.as_ref().unwrap().type_value.as_deref(),
            Some("string")
        );

        let string_optional = property(params, "string_optional");
        assert_eq!(string_optional.type_value.as_deref(), Some("string"));
        assert!(
            !params
                .required
                .as_ref()
                .unwrap()
                .contains(&"string_optional".to_string())
        );

        let array_optional = property(params, "array_optional");
        assert_eq!(array_optional.type_value.as_deref(), Some("array"));
        assert_eq!(
            array_optional.items.as_ref().unwrap().type_value.as_deref(),
            Some("string")
        );
        assert!(
            !params
                .required
                .as_ref()
                .unwrap()
                .contains(&"array_optional".to_string())
        );
    }

    #[test]
    fn test_tool_weather() {
        let source = r#"
import os
from pathlib import Path
from typing import Optional
from urllib.parse import quote_plus
from urllib.request import urlopen


def run(
    location: str,
    llm_output: Optional[str] = None,
) -> str:
    """Get the current weather in a given location

    Args:
        location (str): The city and optionally the state or country (e.g., "London", "San Francisco, CA").

    Returns:
        str: A single-line formatted weather string from wttr.in (``format=4`` with metric units).
    """
    url = f"https://wttr.in/{quote_plus(location)}?format=4&M"

    with urlopen(url, timeout=10) as resp:
        weather = resp.read().decode("utf-8", errors="replace")

    dest = llm_output if llm_output is not None else os.environ.get("LLM_OUTPUT", "/dev/stdout")

    if dest not in {"-", "/dev/stdout"}:
        path = Path(dest)
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as fh:
            fh.write(weather)
    else:
        pass

    return weather
"#;

        let declarations = parse_source(source, "get_current_weather", Path::new("tools")).unwrap();
        assert_eq!(declarations.len(), 1);

        let decl = &declarations[0];
        assert_eq!(decl.name, "get_current_weather");
        assert!(!decl.agent);
        assert!(
            decl.description
                .starts_with("Get the current weather in a given location")
        );

        let params = &decl.parameters;
        assert_eq!(
            params.required.as_ref().unwrap(),
            &vec!["location".to_string()]
        );

        let location = property(params, "location");
        assert_eq!(location.type_value.as_deref(), Some("string"));
        assert_eq!(
            location.description.as_deref(),
            Some(
                "The city and optionally the state or country (e.g., \"London\", \"San Francisco, CA\")."
            )
        );

        let llm_output = property(params, "llm_output");
        assert_eq!(llm_output.type_value.as_deref(), Some("string"));
        assert!(
            !params
                .required
                .as_ref()
                .unwrap()
                .contains(&"llm_output".to_string())
        );
    }

    #[test]
    fn test_tool_execute_py_code() {
        let source = r#"
import ast
import io
from contextlib import redirect_stdout


def run(code: str):
    """Execute the given Python code.
    Args:
        code: The Python code to execute, such as `print("hello world")`
    """
    output = io.StringIO()
    with redirect_stdout(output):
        value = exec_with_return(code, {}, {})

        if value is not None:
            output.write(str(value))

    return output.getvalue()


def exec_with_return(code: str, globals: dict, locals: dict):
    a = ast.parse(code)
    last_expression = None
    if a.body:
        if isinstance(a_last := a.body[-1], ast.Expr):
            last_expression = ast.unparse(a.body.pop())
        elif isinstance(a_last, ast.Assign):
            last_expression = ast.unparse(a_last.targets[0])
        elif isinstance(a_last, (ast.AnnAssign, ast.AugAssign)):
            last_expression = ast.unparse(a_last.target)
    exec(ast.unparse(a), globals, locals)
    if last_expression:
        return eval(last_expression, globals, locals)
"#;

        let declarations = parse_source(source, "execute_py_code", Path::new("tools")).unwrap();
        assert_eq!(declarations.len(), 1);

        let decl = &declarations[0];
        assert_eq!(decl.name, "execute_py_code");
        assert!(!decl.agent);

        let params = &decl.parameters;
        assert_eq!(properties(params).len(), 1);
        let code = property(params, "code");
        assert_eq!(code.type_value.as_deref(), Some("string"));
        assert_eq!(
            code.description.as_deref(),
            Some("The Python code to execute, such as `print(\"hello world\")`")
        );
    }

    #[test]
    fn test_agent_tools() {
        let source = r#"
import urllib.request

def get_ipinfo():
  """
  Get the ip info
  """
  with urllib.request.urlopen("https://httpbin.org/ip") as response:
    data = response.read()
    return data.decode('utf-8')
"#;

        let declarations = parse_source(source, "tools", Path::new("demo")).unwrap();
        assert_eq!(declarations.len(), 1);

        let decl = &declarations[0];
        assert_eq!(decl.name, "get_ipinfo");
        assert!(decl.agent);
        assert_eq!(decl.description, "Get the ip info");
        assert!(properties(&decl.parameters).is_empty());
    }

    #[test]
    fn test_reject_varargs() {
        let source = r#"
def run(*args):
    """Has docstring"""
    return args
"#;

        let err = parse_source(source, "reject_varargs", Path::new("tools")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("*args/*kwargs/positional-only parameters are not supported"));
        assert!(msg.contains("in function 'run'"));
    }

    #[test]
    fn test_reject_kwargs() {
        let source = r#"
def run(**kwargs):
    """Has docstring"""
    return kwargs
"#;

        let err = parse_source(source, "reject_kwargs", Path::new("tools")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("*args/*kwargs/positional-only parameters are not supported"));
        assert!(msg.contains("in function 'run'"));
    }

    #[test]
    fn test_reject_positional_only() {
        let source = r#"
def run(x, /, y):
    """Has docstring"""
    return x + y
"#;

        let err = parse_source(source, "reject_positional_only", Path::new("tools")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("*args/*kwargs/positional-only parameters are not supported"));
        assert!(msg.contains("in function 'run'"));
    }

    #[test]
    fn test_missing_docstring() {
        let source = r#"
def run(x: str):
    pass
"#;

        let err = parse_source(source, "missing_docstring", Path::new("tools")).unwrap_err();
        assert!(
            err.to_string()
                .contains("Missing or empty description on function: run")
        );
    }

    #[test]
    fn test_syntax_error() {
        let source = "def run(: broken";
        let err = parse_source(source, "syntax_error", Path::new("tools")).unwrap_err();
        assert!(err.to_string().contains("failed to parse python"));
    }

    #[test]
    fn test_underscore_functions_skipped() {
        let source = r#"
def _private():
    """Private"""
    return None

def public():
    """Public"""
    return None
"#;

        let declarations = parse_source(source, "tools", Path::new("demo")).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "public");
    }

    #[test]
    fn test_instructions_not_skipped() {
        let source = r#"
def _instructions():
    """Help text"""
    return None
"#;

        let declarations = parse_source(source, "tools", Path::new("demo")).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "instructions");
        assert_eq!(declarations[0].description, "Help text");
        assert!(declarations[0].agent);
    }
}
