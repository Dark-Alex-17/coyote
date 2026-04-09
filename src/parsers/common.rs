use crate::function::{FunctionDeclaration, JsonSchema};
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde_json::Value;
use tree_sitter::Node;

#[derive(Debug)]
pub(crate) struct Param {
    pub name: String,
    pub ty_hint: String,
    pub required: bool,
    pub default: Option<Value>,
    pub doc_type: Option<String>,
    pub doc_desc: Option<String>,
}

pub(crate) trait ScriptedLanguage {
    fn ts_language(&self) -> tree_sitter::Language;

    fn default_runtime(&self) -> &str;

    fn lang_name(&self) -> &str;

    fn find_functions<'a>(
        &self,
        root: Node<'a>,
        src: &str,
    ) -> Vec<(Node<'a>, Node<'a>)>;

    fn function_name<'a>(&self, func_node: Node<'a>, src: &'a str) -> Result<&'a str>;

    fn extract_description(
        &self,
        wrapper_node: Node<'_>,
        func_node: Node<'_>,
        src: &str,
    ) -> Option<String>;

    fn extract_params(
        &self,
        func_node: Node<'_>,
        src: &str,
        description: &str,
    ) -> Result<Vec<Param>>;
}

pub(crate) fn build_param(
    name: &str,
    mut ty: String,
    mut required: bool,
    default: Option<Value>,
) -> Param {
    if ty.ends_with('?') {
        ty.pop();
        required = false;
    }

    Param {
        name: name.to_string(),
        ty_hint: ty,
        required,
        default,
        doc_type: None,
        doc_desc: None,
    }
}

pub(crate) fn build_parameters_schema(params: &[Param], _description: &str) -> JsonSchema {
    let mut props: IndexMap<String, JsonSchema> = IndexMap::new();
    let mut req: Vec<String> = Vec::new();

    for p in params {
        let name = p.name.replace('-', "_");
        let mut schema = JsonSchema::default();

        let ty = if !p.ty_hint.is_empty() {
            p.ty_hint.as_str()
        } else if let Some(t) = &p.doc_type {
            t.as_str()
        } else {
            "str"
        };

        if let Some(d) = &p.doc_desc
            && !d.is_empty()
        {
            schema.description = Some(d.clone());
        }

        apply_type_to_schema(ty, &mut schema);

        if p.default.is_none() && p.required {
            req.push(name.clone());
        }

        props.insert(name, schema);
    }

    JsonSchema {
        type_value: Some("object".into()),
        description: None,
        properties: Some(props),
        items: None,
        any_of: None,
        enum_value: None,
        default: None,
        required: if req.is_empty() { None } else { Some(req) },
    }
}

pub(crate) fn apply_type_to_schema(ty: &str, s: &mut JsonSchema) {
    let t = ty.trim_end_matches('?');
    if let Some(rest) = t.strip_prefix("list[") {
        s.type_value = Some("array".into());
        let inner = rest.trim_end_matches(']');
        let mut item = JsonSchema::default();

        apply_type_to_schema(inner, &mut item);

        if item.type_value.is_none() {
            item.type_value = Some("string".into());
        }
        s.items = Some(Box::new(item));

        return;
    }

    if let Some(rest) = t.strip_prefix("literal:") {
        s.type_value = Some("string".into());
        let vals = rest
            .split('|')
            .map(|x| x.trim().trim_matches('"').trim_matches('\'').to_string())
            .collect::<Vec<_>>();
        if !vals.is_empty() {
            s.enum_value = Some(vals);
        }
        return;
    }

    s.type_value = Some(
        match t {
            "bool" => "boolean",
            "int" => "integer",
            "float" => "number",
            "str" | "any" | "" => "string",
            _ => "string",
        }
        .into(),
    );
}

pub(crate) fn underscore(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

pub(crate) fn node_text<'a>(node: Node<'_>, src: &'a str) -> Result<&'a str> {
    node.utf8_text(src.as_bytes())
        .map_err(|err| anyhow!("invalid utf-8 in source: {err}"))
}

pub(crate) fn named_child(node: Node<'_>, index: usize) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).nth(index)
}

pub(crate) fn extract_runtime(tree: &tree_sitter::Tree, src: &str, default: &str) -> String {
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let text = match child.kind() {
            "hash_bang_line" | "comment" => match child.utf8_text(src.as_bytes()) {
                Ok(t) => t,
                Err(_) => continue,
            },
            _ => break,
        };

        if let Some(cmd) = text.strip_prefix("#!") {
            let cmd = cmd.trim();
            if let Some(after_env) = cmd.strip_prefix("/usr/bin/env ") {
                return after_env.trim().to_string();
            }
            return cmd.to_string();
        }

        break;
    }
    default.to_string()
}

pub(crate) fn generate_declarations<L: ScriptedLanguage>(
    lang: &L,
    src: &str,
    file_name: &str,
    is_tool: bool,
) -> Result<Vec<FunctionDeclaration>> {
    let mut parser = tree_sitter::Parser::new();
    let language = lang.ts_language();
    parser.set_language(&language).with_context(|| {
        format!(
            "failed to initialize {} tree-sitter parser",
            lang.lang_name()
        )
    })?;

    let tree = parser
        .parse(src.as_bytes(), None)
        .ok_or_else(|| anyhow!("failed to parse {}: {file_name}", lang.lang_name()))?;

    if tree.root_node().has_error() {
        bail!(
            "failed to parse {}: syntax error in {file_name}",
            lang.lang_name()
        );
    }

    let _runtime = extract_runtime(&tree, src, lang.default_runtime());

    let mut out = Vec::new();
    for (wrapper, func) in lang.find_functions(tree.root_node(), src) {
        let func_name = lang.function_name(func, src)?;

        if func_name.starts_with('_') && func_name != "_instructions" {
            continue;
        }
        if is_tool && func_name != "run" {
            continue;
        }

        let description = lang
            .extract_description(wrapper, func, src)
            .unwrap_or_default();
        let params = lang
            .extract_params(func, src, &description)
            .with_context(|| format!("in function '{func_name}' in {file_name}"))?;
        let schema = build_parameters_schema(&params, &description);

        let name = if is_tool && func_name == "run" {
            underscore(file_name)
        } else {
            underscore(func_name)
        };

        let desc_trim = description.trim().to_string();
        if desc_trim.is_empty() {
            bail!("Missing or empty description on function: {func_name}");
        }

        out.push(FunctionDeclaration {
            name,
            description: desc_trim,
            parameters: schema,
            agent: !is_tool,
        });
    }
    Ok(out)
}
