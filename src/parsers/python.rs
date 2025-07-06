use crate::function::{FunctionDeclaration, JsonSchema};
use anyhow::{bail, Context, Result};
use ast::{Stmt, StmtFunctionDef};
use indexmap::IndexMap;
use rustpython_ast::{Constant, Expr, UnaryOp};
use rustpython_parser::{ast, Mode};
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug)]
struct Param {
    name: String,
    ty_hint: String,
    required: bool,
    default: Option<Value>,
    doc_type: Option<String>,
    doc_desc: Option<String>,
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
    let suite = parse_suite(&src, file_name)?;

    let is_tool = parent
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "tools");
    let mut declarations = python_to_function_declarations(file_name, &suite, is_tool)?;

    if is_tool {
        for d in &mut declarations {
            d.agent = true;
        }
    }

    Ok(declarations)
}

fn parse_suite(src: &str, filename: &str) -> Result<ast::Suite> {
    let mod_ast =
        rustpython_parser::parse(src, Mode::Module, filename).context("failed to parse python")?;

    let suite = match mod_ast {
        ast::Mod::Module(m) => m.body,
        ast::Mod::Interactive(m) => m.body,
        ast::Mod::Expression(_) => bail!("expected a module; got a single expression"),
        _ => bail!("unexpected parse mode/AST variant"),
    };

    Ok(suite)
}

fn python_to_function_declarations(
    file_name: &str,
    module: &ast::Suite,
    is_tool: bool,
) -> Result<Vec<FunctionDeclaration>> {
    let mut out = Vec::new();

    for stmt in module {
        if let Stmt::FunctionDef(fd) = stmt {
            let func_name = fd.name.to_string();

            if func_name.starts_with('_') && func_name != "_instructions" {
                continue;
            }

            if is_tool && func_name != "run" {
                continue;
            }

            let description = get_docstring_from_body(&fd.body).unwrap_or_default();
            let params = collect_params(fd);
            let schema = build_parameters_schema(&params, &description);
            let name = if is_tool && func_name == "run" {
                underscore(file_name)
            } else {
                underscore(&func_name)
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
    }

    Ok(out)
}

fn get_docstring_from_body(body: &[Stmt]) -> Option<String> {
    let first = body.first()?;
    if let Stmt::Expr(expr_stmt) = first {
        if let Expr::Constant(constant) = &*expr_stmt.value {
            if let Constant::Str(s) = &constant.value {
                return Some(s.clone());
            }
        }
    }
    None
}

fn collect_params(fd: &StmtFunctionDef) -> Vec<Param> {
    let mut out = Vec::new();

    for a in fd.args.posonlyargs.iter().chain(fd.args.args.iter()) {
        let name = a.def.arg.to_string();
        let mut ty = get_arg_type(a.def.annotation.as_deref());
        let mut required = a.default.is_none();

        if ty.ends_with('?') {
            ty.pop();
            required = false;
        }

        let default = if a.default.is_some() {
            Some(Value::Null)
        } else {
            None
        };

        out.push(Param {
            name,
            ty_hint: ty,
            required,
            default,
            doc_type: None,
            doc_desc: None,
        });
    }

    for a in &fd.args.kwonlyargs {
        let name = a.def.arg.to_string();
        let mut ty = get_arg_type(a.def.annotation.as_deref());
        let mut required = a.default.is_none();

        if ty.ends_with('?') {
            ty.pop();
            required = false;
        }

        let default = if a.default.is_some() {
            Some(Value::Null)
        } else {
            None
        };

        out.push(Param {
            name,
            ty_hint: ty,
            required,
            default,
            doc_type: None,
            doc_desc: None,
        });
    }

    if let Some(vararg) = &fd.args.vararg {
        let name = vararg.arg.to_string();
        let inner = get_arg_type(vararg.annotation.as_deref());
        let ty = if inner.is_empty() {
            "list[str]".into()
        } else {
            format!("list[{inner}]")
        };

        out.push(Param {
            name,
            ty_hint: ty,
            required: false,
            default: None,
            doc_type: None,
            doc_desc: None,
        });
    }

    if let Some(kwarg) = &fd.args.kwarg {
        let name = kwarg.arg.to_string();
        out.push(Param {
            name,
            ty_hint: "object".into(),
            required: false,
            default: None,
            doc_type: None,
            doc_desc: None,
        });
    }

    if let Some(doc) = get_docstring_from_body(&fd.body) {
        let meta = parse_docstring_args(&doc);
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
    }

    out
}

fn get_arg_type(annotation: Option<&Expr>) -> String {
    match annotation {
        None => "".to_string(),
        Some(Expr::Name(n)) => n.id.to_string(),
        Some(Expr::Subscript(sub)) => match &*sub.value {
            Expr::Name(name) if &name.id == "Optional" => {
                let inner = get_arg_type(Some(&sub.slice));
                format!("{inner}?")
            }
            Expr::Name(name) if &name.id == "List" => {
                let inner = get_arg_type(Some(&sub.slice));
                format!("list[{inner}]")
            }
            Expr::Name(name) if &name.id == "Literal" => {
                let vals = literal_members(&sub.slice);
                format!("literal:{}", vals.join("|"))
            }
            _ => "any".to_string(),
        },
        _ => "any".to_string(),
    }
}

fn expr_to_str(e: &Expr) -> String {
    match e {
        Expr::Constant(c) => match &c.value {
            Constant::Str(s) => s.clone(),
            Constant::Int(i) => i.to_string(),
            Constant::Float(f) => f.to_string(),
            Constant::Bool(b) => b.to_string(),
            Constant::None => "None".to_string(),
            Constant::Ellipsis => "...".to_string(),
            Constant::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            Constant::Complex { real, imag } => format!("{real}+{imag}j"),
            _ => "any".to_string(),
        },

        Expr::Name(n) => n.id.to_string(),

        Expr::UnaryOp(u) => {
            if matches!(u.op, UnaryOp::USub) {
                let inner = expr_to_str(&u.operand);
                if inner.parse::<f64>().is_ok() || inner.chars().all(|c| c.is_ascii_digit()) {
                    return format!("-{inner}");
                }
            }
            "any".to_string()
        }

        Expr::Tuple(t) => t.elts.iter().map(expr_to_str).collect::<Vec<_>>().join(","),

        _ => "any".to_string(),
    }
}

fn literal_members(e: &Expr) -> Vec<String> {
    match e {
        Expr::Tuple(t) => t.elts.iter().map(expr_to_str).collect(),
        _ => vec![expr_to_str(e)],
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

fn underscore(s: &str) -> String {
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

fn build_parameters_schema(params: &[Param], _description: &str) -> JsonSchema {
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

        if let Some(d) = &p.doc_desc {
            if !d.is_empty() {
                schema.description = Some(d.clone());
            }
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

fn apply_type_to_schema(ty: &str, s: &mut JsonSchema) {
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
