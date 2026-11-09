#![cfg_attr( // TASK-436
    not(test),
    expect(
        dead_code,
        reason = "consumed by the provider request builders and graph structured-output path"
    )
)]

use serde_json::{Map, Value, json};

const MAX_SCHEMA_NAME_LEN: usize = 64;
const DEFAULT_SCHEMA_NAME: &str = "output";
const MAX_REF_DEPTH: usize = 32;

const OPENAI_STRICT_KEPT: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "const",
    "description",
    "title",
    "anyOf",
    "$defs",
    "definitions",
    "$ref",
    "additionalProperties",
    "pattern",
    "format",
];

const ANTHROPIC_KEPT: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "const",
    "default",
    "description",
    "title",
    "anyOf",
    "allOf",
    "$ref",
    "$defs",
    "definitions",
    "additionalProperties",
    "pattern",
    "format",
    "minItems",
];

const ANTHROPIC_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

const DROPPED: &[&str] = &["$schema", "$id", "$comment"];

#[derive(Clone, Debug, PartialEq)]
pub struct StructuredOutputSpec {
    pub name: String,
    pub schema: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaDialect {
    OpenAiStrict,
    Anthropic,
    Gemini,
}

/// Rewrites a JSON Schema into the subset a provider accepts. Keywords the
/// dialect cannot express are rendered into the node's `description` so the
/// model still sees them.
pub fn convert_schema(schema: &Value, dialect: SchemaDialect) -> Value {
    match dialect {
        SchemaDialect::Gemini => schema.clone(),
        SchemaDialect::OpenAiStrict | SchemaDialect::Anthropic => convert_node(schema, dialect),
    }
}

/// Maps an arbitrary label onto the `[A-Za-z0-9_-]{1,64}` shape providers
/// require for a response-format name.
pub fn sanitize_schema_name(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_SCHEMA_NAME_LEN)
        .collect();
    if sanitized.is_empty() {
        DEFAULT_SCHEMA_NAME.to_string()
    } else {
        sanitized
    }
}

/// Removes `null` values that a strict dialect forced the model to emit for
/// optional properties, restoring the shape the original schema describes.
pub fn strip_injected_nulls(value: &mut Value, original_schema: &Value) {
    strip_node(value, original_schema, original_schema, 0);
}

fn convert_node(node: &Value, dialect: SchemaDialect) -> Value {
    let Value::Object(map) = node else {
        return node.clone();
    };

    let mut out = Map::new();
    let mut moved: Vec<(&str, &Value)> = Vec::new();
    let has_any_of = map.contains_key("anyOf");

    for (key, value) in map {
        let key = key.as_str();
        if DROPPED.contains(&key) {
            continue;
        }
        if key == "oneOf" {
            if has_any_of {
                moved.push((key, value));
            } else {
                out.insert("anyOf".to_string(), convert_each(value, dialect));
            }
            continue;
        }
        if key == "description" && !value.is_string() {
            moved.push((key, value));
            continue;
        }
        if !is_kept(key, value, dialect) {
            moved.push((key, value));
            continue;
        }
        let converted = match key {
            "properties" | "$defs" | "definitions" => convert_values(value, dialect),
            "items" if value.is_array() => convert_each(value, dialect),
            "items" | "additionalProperties" => convert_node(value, dialect),
            "anyOf" | "allOf" => convert_each(value, dialect),
            _ => value.clone(),
        };
        out.insert(key.to_string(), converted);
    }

    if !moved.is_empty() {
        append_moved_to_description(&mut out, &moved);
    }

    if is_object_schema(map) {
        out.entry("additionalProperties")
            .or_insert(Value::Bool(false));
        if dialect == SchemaDialect::OpenAiStrict {
            make_strict_object(&mut out, map);
        }
    }

    Value::Object(out)
}

fn is_kept(key: &str, value: &Value, dialect: SchemaDialect) -> bool {
    match dialect {
        SchemaDialect::OpenAiStrict => OPENAI_STRICT_KEPT.contains(&key),
        SchemaDialect::Anthropic => match key {
            "format" => value
                .as_str()
                .is_some_and(|format| ANTHROPIC_FORMATS.contains(&format)),
            "minItems" => matches!(value.as_u64(), Some(0 | 1)),
            "allOf" => value
                .as_array()
                .is_some_and(|entries| !entries.iter().any(|entry| entry.get("$ref").is_some())),
            _ => ANTHROPIC_KEPT.contains(&key),
        },
        SchemaDialect::Gemini => true,
    }
}

fn convert_values(value: &Value, dialect: SchemaDialect) -> Value {
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(name, schema)| (name.clone(), convert_node(schema, dialect)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn convert_each(value: &Value, dialect: SchemaDialect) -> Value {
    match value {
        Value::Array(entries) => Value::Array(
            entries
                .iter()
                .map(|schema| convert_node(schema, dialect))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn append_moved_to_description(out: &mut Map<String, Value>, moved: &[(&str, &Value)]) {
    let rendered = moved
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let block = format!("{{{rendered}}}");
    let description = match out.get("description").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{existing}\n\n{block}"),
        _ => block,
    };
    out.insert("description".to_string(), Value::String(description));
}

fn is_object_schema(map: &Map<String, Value>) -> bool {
    map.contains_key("properties")
        || match map.get("type") {
            Some(Value::String(t)) => t == "object",
            Some(Value::Array(types)) => types.iter().any(|t| t == "object"),
            _ => false,
        }
}

fn make_strict_object(out: &mut Map<String, Value>, original: &Map<String, Value>) {
    let originally_required = required_names(original);
    let Some(Value::Object(properties)) = out.get_mut("properties") else {
        out.insert("required".to_string(), json!([]));
        return;
    };
    let mut all_keys = Vec::with_capacity(properties.len());
    for (name, schema) in properties.iter_mut() {
        if !originally_required.contains(&name.as_str()) {
            make_nullable(schema);
        }
        all_keys.push(Value::String(name.clone()));
    }
    out.insert("required".to_string(), Value::Array(all_keys));
}

fn required_names(map: &Map<String, Value>) -> Vec<&str> {
    map.get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn make_nullable(schema: &mut Value) {
    let Value::Object(map) = schema else {
        return;
    };
    if let Some(Value::Array(variants)) = map.get_mut("enum")
        && !variants.contains(&Value::Null)
    {
        variants.push(Value::Null);
    }
    match map.get_mut("type") {
        Some(t @ Value::String(_)) => {
            if t != "null" {
                *t = json!([std::mem::take(t), "null"]);
            }
        }
        Some(Value::Array(types)) => {
            if !types.iter().any(|t| t == "null") {
                types.push(json!("null"));
            }
        }
        Some(_) => {}
        None => {
            if !has_null_branch(map, "anyOf") {
                let inner = std::mem::take(schema);
                *schema = json!({ "anyOf": [inner, { "type": "null" }] });
            }
        }
    }
}

fn has_null_branch(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key)
        .and_then(Value::as_array)
        .is_some_and(|branches| {
            branches
                .iter()
                .any(|branch| branch.get("type").is_some_and(type_allows_null))
        })
}

fn type_allows_null(t: &Value) -> bool {
    match t {
        Value::String(t) => t == "null",
        Value::Array(types) => types.iter().any(|t| t == "null"),
        _ => false,
    }
}

fn resolve_ref<'a>(schema: &'a Value, root: &'a Value, depth: &mut usize) -> Option<&'a Value> {
    let mut current = schema;
    while let Some(reference) = current.get("$ref").and_then(Value::as_str) {
        if *depth >= MAX_REF_DEPTH {
            return None;
        }
        *depth += 1;
        current = lookup_local_ref(reference, root)?;
    }
    Some(current)
}

fn lookup_local_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    let (container, name) = reference
        .strip_prefix("#/$defs/")
        .map(|name| ("$defs", name))
        .or_else(|| {
            reference
                .strip_prefix("#/definitions/")
                .map(|name| ("definitions", name))
        })?;
    root.get(container)?.get(name)
}

fn allows_null(schema: &Value, root: &Value, depth: usize) -> bool {
    let mut depth = depth;
    let Some(schema) = resolve_ref(schema, root, &mut depth) else {
        return true;
    };
    let map = match schema {
        Value::Bool(accepts_anything) => return *accepts_anything,
        Value::Object(map) => map,
        _ => return true,
    };
    if map.get("type").is_some_and(type_allows_null)
        || map
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|variants| variants.contains(&Value::Null))
        || map.get("const").is_some_and(Value::is_null)
    {
        return true;
    }
    let branch_allows_null = |key: &str| {
        map.get(key)
            .and_then(Value::as_array)
            .is_some_and(|branches| {
                branches
                    .iter()
                    .any(|branch| allows_null(branch, root, depth + 1))
            })
    };
    if branch_allows_null("anyOf") || branch_allows_null("oneOf") {
        return true;
    }
    !["type", "enum", "const", "anyOf", "oneOf", "allOf", "not"]
        .iter()
        .any(|key| map.contains_key(*key))
}

fn strip_node(value: &mut Value, schema: &Value, root: &Value, depth: usize) {
    let mut depth = depth;
    let Some(schema) = resolve_ref(schema, root, &mut depth) else {
        return;
    };
    match value {
        Value::Object(fields) => {
            let Some(Value::Object(properties)) = schema.get("properties") else {
                return;
            };
            let required = schema.as_object().map(required_names).unwrap_or_default();
            fields.retain(|name, field| {
                let Some(property) = properties.get(name) else {
                    return true;
                };
                !(field.is_null()
                    && !required.contains(&name.as_str())
                    && !allows_null(property, root, depth))
            });
            for (name, field) in fields.iter_mut() {
                if let Some(property) = properties.get(name) {
                    strip_node(field, property, root, depth);
                }
            }
        }
        Value::Array(elements) => match schema.get("items") {
            Some(Value::Array(tuple)) => {
                for (element, item_schema) in elements.iter_mut().zip(tuple) {
                    strip_node(element, item_schema, root, depth);
                }
            }
            Some(item_schema) => {
                for element in elements.iter_mut() {
                    strip_node(element, item_schema, root, depth);
                }
            }
            None => {}
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn code_reviewer_groups_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "groups": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "domain": { "type": "string" },
                            "files": { "type": "array", "items": { "type": "string" } },
                            "rationale": { "type": "string" }
                        },
                        "required": ["domain", "files"]
                    }
                }
            },
            "required": ["groups"]
        })
    }

    #[test]
    fn openai_strict_closes_and_requires_the_groups_schema() {
        let converted = convert_schema(&code_reviewer_groups_schema(), SchemaDialect::OpenAiStrict);

        let items = &converted["properties"]["groups"]["items"];
        assert_eq!(items["additionalProperties"], json!(false));
        assert_eq!(items["required"], json!(["domain", "files", "rationale"]));
        assert_eq!(
            items["properties"]["rationale"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(items["properties"]["domain"]["type"], json!("string"));
        assert_eq!(converted["required"], json!(["groups"]));
        assert_eq!(converted["additionalProperties"], json!(false));
    }

    #[test]
    fn openai_strict_required_follows_declaration_order() {
        let schema = json!({
            "type": "object",
            "properties": { "zeta": { "type": "string" }, "alpha": { "type": "string" } },
            "required": ["alpha", "zeta"]
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(converted["required"], json!(["zeta", "alpha"]));
    }

    #[test]
    fn range_constraints_move_into_existing_description() {
        let schema = json!({
            "type": "integer",
            "description": "Complexity",
            "minimum": 1,
            "maximum": 10
        });

        for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
            let converted = convert_schema(&schema, dialect);
            assert_eq!(
                converted,
                json!({
                    "type": "integer",
                    "description": "Complexity\n\n{minimum: 1, maximum: 10}"
                }),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn range_constraints_without_description_have_no_leading_blank_lines() {
        let schema = json!({ "type": "integer", "minimum": 1, "maximum": 10 });

        for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
            let converted = convert_schema(&schema, dialect);
            assert_eq!(
                converted["description"],
                json!("{minimum: 1, maximum: 10}"),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn moved_values_use_json_rendering() {
        let schema = json!({
            "type": "array",
            "items": { "type": "string" },
            "uniqueItems": true,
            "default": ["a"]
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(
            converted["description"],
            json!("{uniqueItems: true, default: [\"a\"]}")
        );
    }

    #[test]
    fn anthropic_keeps_supported_keywords() {
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": { "const": "x" },
                "mode": { "type": "string", "default": "fast", "pattern": "^[a-z]+$" },
                "when": { "type": "string", "format": "date-time" },
                "none": { "type": "array", "items": { "type": "string" }, "minItems": 0 },
                "one": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
            }
        });

        let converted = convert_schema(&schema, SchemaDialect::Anthropic);
        let properties = &converted["properties"];

        assert_eq!(properties["kind"], json!({ "const": "x" }));
        assert_eq!(
            properties["mode"],
            json!({ "type": "string", "default": "fast", "pattern": "^[a-z]+$" })
        );
        assert_eq!(
            properties["when"],
            json!({ "type": "string", "format": "date-time" })
        );
        assert_eq!(properties["none"]["minItems"], json!(0));
        assert_eq!(properties["one"]["minItems"], json!(1));
        assert!(converted.get("required").is_none());
    }

    #[test]
    fn anthropic_moves_unsupported_keywords() {
        let schema = json!({
            "type": "object",
            "properties": {
                "many": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 2,
                    "maxItems": 5
                },
                "blob": { "type": "string", "format": "binary" },
                "name": { "type": "string", "minLength": 3 }
            }
        });

        let converted = convert_schema(&schema, SchemaDialect::Anthropic);
        let properties = &converted["properties"];

        assert_eq!(
            properties["many"],
            json!({
                "type": "array",
                "items": { "type": "string" },
                "description": "{minItems: 2, maxItems: 5}"
            })
        );
        assert_eq!(
            properties["blob"],
            json!({ "type": "string", "description": "{format: \"binary\"}" })
        );
        assert_eq!(
            properties["name"],
            json!({ "type": "string", "description": "{minLength: 3}" })
        );
    }

    #[test]
    fn one_of_becomes_any_of() {
        let schema = json!({ "oneOf": [{ "type": "string" }, { "type": "integer" }] });

        for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
            let converted = convert_schema(&schema, dialect);
            assert_eq!(
                converted,
                json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }] }),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn anthropic_moves_all_of_with_ref_entries() {
        let schema = json!({
            "allOf": [{ "$ref": "#/$defs/base" }],
            "$defs": { "base": { "type": "string" } }
        });

        let converted = convert_schema(&schema, SchemaDialect::Anthropic);

        assert!(converted.get("allOf").is_none());
        assert_eq!(
            converted["description"],
            json!("{allOf: [{\"$ref\":\"#/$defs/base\"}]}")
        );
    }

    #[test]
    fn gemini_returns_input_unchanged() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "n": { "type": "integer", "minimum": 1, "maximum": 10 } },
            "oneOf": [{ "required": ["n"] }]
        });

        assert_eq!(convert_schema(&schema, SchemaDialect::Gemini), schema);
    }

    #[test]
    fn explicit_additional_properties_is_preserved() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["a"],
            "additionalProperties": true
        });

        for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
            let converted = convert_schema(&schema, dialect);
            assert_eq!(
                converted["additionalProperties"],
                json!(true),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn schema_metadata_keywords_are_dropped() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "urn:example",
            "$comment": "internal",
            "type": "string"
        });

        for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
            let converted = convert_schema(&schema, dialect);
            assert_eq!(converted, json!({ "type": "string" }), "{dialect:?}");
        }
    }

    #[test]
    fn openai_strict_makes_optional_properties_nullable() {
        let schema = json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" },
                "multi": { "type": ["string", "integer"] },
                "already": { "type": ["string", "null"] },
                "choice": { "enum": ["a", "b"] },
                "typed_choice": { "type": "string", "enum": ["a", "b"] },
                "needed": { "type": "string" }
            },
            "required": ["needed"]
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);
        let properties = &converted["properties"];

        assert_eq!(properties["text"]["type"], json!(["string", "null"]));
        assert_eq!(
            properties["multi"]["type"],
            json!(["string", "integer", "null"])
        );
        assert_eq!(properties["already"]["type"], json!(["string", "null"]));
        assert_eq!(
            properties["choice"],
            json!({ "anyOf": [{ "enum": ["a", "b", null] }, { "type": "null" }] })
        );
        assert_eq!(
            properties["typed_choice"],
            json!({ "type": ["string", "null"], "enum": ["a", "b", null] })
        );
        assert_eq!(properties["needed"]["type"], json!("string"));
    }

    #[test]
    fn sanitize_schema_name_replaces_invalid_characters() {
        assert_eq!(sanitize_schema_name("my node.v2"), "my_node_v2");
        assert_eq!(sanitize_schema_name("keep-this_1"), "keep-this_1");
    }

    #[test]
    fn sanitize_schema_name_truncates_to_64_characters() {
        let raw = "a".repeat(70);

        assert_eq!(sanitize_schema_name(&raw), "a".repeat(64));
    }

    #[test]
    fn sanitize_schema_name_defaults_when_empty() {
        assert_eq!(sanitize_schema_name(""), "output");
    }

    #[test]
    fn spec_carries_sanitized_name_and_converted_schema() {
        let schema = json!({ "type": "string", "minLength": 1 });
        let spec = StructuredOutputSpec {
            name: sanitize_schema_name("review groups"),
            schema: convert_schema(&schema, SchemaDialect::OpenAiStrict),
        };

        assert_eq!(
            spec.clone(),
            StructuredOutputSpec {
                name: "review_groups".to_string(),
                schema: json!({ "type": "string", "description": "{minLength: 1}" }),
            }
        );
    }

    #[test]
    fn strip_injected_nulls_removes_optional_nulls_in_array_items() {
        let mut value = json!({
            "groups": [
                { "domain": "client", "files": ["a.rs"], "rationale": null },
                { "domain": "graph", "files": [], "rationale": "shared state" }
            ]
        });

        strip_injected_nulls(&mut value, &code_reviewer_groups_schema());

        assert_eq!(
            value,
            json!({
                "groups": [
                    { "domain": "client", "files": ["a.rs"] },
                    { "domain": "graph", "files": [], "rationale": "shared state" }
                ]
            })
        );
    }

    #[test]
    fn strip_injected_nulls_keeps_nulls_the_schema_allows() {
        let schema = json!({
            "type": "object",
            "properties": {
                "needed": { "type": "string" },
                "nullable": { "type": ["string", "null"] },
                "nullable_enum": { "enum": ["a", null] },
                "nullable_branch": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                "dropped": { "type": "string" }
            },
            "required": ["needed"]
        });
        let mut value = json!({
            "needed": null,
            "nullable": null,
            "nullable_enum": null,
            "nullable_branch": null,
            "dropped": null
        });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(
            value,
            json!({
                "needed": null,
                "nullable": null,
                "nullable_enum": null,
                "nullable_branch": null
            })
        );
    }

    #[test]
    fn strip_injected_nulls_follows_local_refs() {
        let schema = json!({
            "type": "object",
            "properties": { "inner": { "$ref": "#/$defs/x" } },
            "required": ["inner"],
            "$defs": {
                "x": {
                    "type": "object",
                    "properties": { "note": { "type": "string" } }
                }
            }
        });
        let mut value = json!({ "inner": { "note": null } });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "inner": {} }));
    }

    #[test]
    fn strip_injected_nulls_terminates_on_self_recursive_schema() {
        let schema = json!({
            "$ref": "#/definitions/node",
            "definitions": {
                "node": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "child": { "$ref": "#/definitions/node" },
                        "loop": { "$ref": "#/definitions/loop" }
                    }
                },
                "loop": { "$ref": "#/definitions/loop" }
            }
        });
        let mut value = json!({
            "label": null,
            "child": { "label": "leaf", "child": { "label": null } },
            "loop": { "label": null }
        });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(
            value,
            json!({
                "child": { "label": "leaf", "child": {} },
                "loop": { "label": null }
            })
        );
    }

    #[test]
    fn recursion_covers_properties() {
        let schema = json!({
            "type": "object",
            "properties": { "n": { "type": "integer", "minimum": 1 } },
            "required": ["n"]
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(
            converted["properties"]["n"],
            json!({ "type": "integer", "description": "{minimum: 1}" })
        );
    }

    #[test]
    fn recursion_covers_items() {
        let schema = json!({ "type": "array", "items": { "type": "string", "minLength": 2 } });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(
            converted["items"],
            json!({ "type": "string", "description": "{minLength: 2}" })
        );
    }

    #[test]
    fn recursion_covers_any_of() {
        let schema = json!({ "anyOf": [{ "type": "string", "maxLength": 4 }, { "type": "null" }] });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(
            converted["anyOf"][0],
            json!({ "type": "string", "description": "{maxLength: 4}" })
        );
    }

    #[test]
    fn recursion_covers_one_of() {
        let schema = json!({ "oneOf": [{ "type": "integer", "multipleOf": 5 }] });

        let converted = convert_schema(&schema, SchemaDialect::Anthropic);

        assert_eq!(
            converted["anyOf"][0],
            json!({ "type": "integer", "description": "{multipleOf: 5}" })
        );
    }

    #[test]
    fn recursion_covers_all_of() {
        let schema = json!({
            "allOf": [{ "type": "object", "properties": { "a": { "type": "string" } } }]
        });

        let converted = convert_schema(&schema, SchemaDialect::Anthropic);

        assert_eq!(
            converted["allOf"][0],
            json!({
                "type": "object",
                "properties": { "a": { "type": "string" } },
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn recursion_covers_defs() {
        let schema = json!({
            "$ref": "#/$defs/item",
            "$defs": { "item": { "type": "object", "properties": { "a": { "type": "string" } } } }
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(
            converted["$defs"]["item"],
            json!({
                "type": "object",
                "properties": { "a": { "type": ["string", "null"] } },
                "additionalProperties": false,
                "required": ["a"]
            })
        );
    }

    #[test]
    fn recursion_covers_definitions() {
        let schema = json!({
            "$ref": "#/definitions/item",
            "definitions": { "item": { "type": "number", "exclusiveMinimum": 0 } }
        });

        let converted = convert_schema(&schema, SchemaDialect::Anthropic);

        assert_eq!(
            converted["definitions"]["item"],
            json!({ "type": "number", "description": "{exclusiveMinimum: 0}" })
        );
    }
}
