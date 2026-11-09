use serde_json::{Map, Value, json};

const MAX_SCHEMA_NAME_LEN: usize = 64;
const DEFAULT_SCHEMA_NAME: &str = "output";
const MAX_REF_DEPTH: usize = 32;
const ALLOWS_NULL_BUDGET: usize = 4096;
const EXPANSION_BUDGET: usize = 1024;
const UNMODELLED_KEYWORDS: &[&str] = &[
    "prefixItems",
    "patternProperties",
    "if",
    "dependentRequired",
    "dependentSchemas",
    "unevaluatedProperties",
    "unevaluatedItems",
];

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

// `format`, `minItems` and `allOf` are additionally gated by value in `is_kept`.
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
/// A `null` is removed only when the original schema certainly rejects it;
/// anything undecidable keeps the value as the model produced it.
pub fn strip_injected_nulls(value: &mut Value, original_schema: &Value) {
    let constraint = Constraint {
        schemas: vec![original_schema],
        unions: Vec::new(),
    };
    strip_value(value, &constraint, original_schema);
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
        // Closing an object that declares no properties would make the
        // provider enforce `{}` and drop its data. Left open; providers that
        // require closed objects reject the schema and the caller resends
        // without the format.
        if map
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| !properties.is_empty())
        {
            out.entry("additionalProperties")
                .or_insert(Value::Bool(false));
        }
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
    let already_admits_null = admits_null_directly(map);
    match map.get("const").map(Value::is_null) {
        Some(true) => return,
        Some(false) => {
            wrap_nullable(schema);
            return;
        }
        None => {}
    }
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
        None if already_admits_null => {}
        None => match map.get_mut("anyOf") {
            Some(Value::Array(branches)) => {
                if !has_null_branch(branches) {
                    branches.push(json!({ "type": "null" }));
                }
            }
            _ => wrap_nullable(schema),
        },
    }
}

fn wrap_nullable(schema: &mut Value) {
    let inner = std::mem::take(schema);
    *schema = json!({ "anyOf": [inner, { "type": "null" }] });
}

fn has_null_branch(branches: &[Value]) -> bool {
    branches
        .iter()
        .any(|branch| branch.as_object().is_some_and(admits_null_directly))
}

fn admits_null_directly(map: &Map<String, Value>) -> bool {
    map.get("type").is_some_and(type_allows_null)
        || map
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|variants| variants.contains(&Value::Null))
        || map.get("const").is_some_and(Value::is_null)
}

fn type_allows_null(t: &Value) -> bool {
    match t {
        Value::String(t) => t == "null",
        Value::Array(types) => types.iter().any(|t| t == "null"),
        _ => false,
    }
}

/// Follows one `$ref` hop from `map`. `None` means the reference cannot be
/// followed (non-local, missing, or past the cycle guard); `Some(None)` means
/// `map` has no reference at all.
fn ref_target<'a>(
    map: &Map<String, Value>,
    root: &'a Value,
    chain: usize,
) -> Option<Option<&'a Value>> {
    let Some(reference) = map.get("$ref").and_then(Value::as_str) else {
        return Some(None);
    };
    if chain >= MAX_REF_DEPTH {
        return None;
    }
    lookup_local_ref(reference, root).map(Some)
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

/// Answers `false` only when every keyword that can reject `null` was evaluated
/// and one of them does; an exhausted budget or an unresolvable reference
/// answers `true`.
fn allows_null(schema: &Value, root: &Value, chain: usize, budget: &mut usize) -> bool {
    if *budget == 0 {
        return true;
    }
    *budget -= 1;
    let map = match schema {
        Value::Object(map) => map,
        Value::Bool(accepts_anything) => return *accepts_anything,
        _ => return true,
    };
    if map.contains_key("not") {
        return true;
    }
    if map.get("type").is_some_and(|t| !type_allows_null(t)) {
        return false;
    }
    if let Some(Value::Array(variants)) = map.get("enum")
        && !variants.contains(&Value::Null)
    {
        return false;
    }
    if map.get("const").is_some_and(|c| !c.is_null()) {
        return false;
    }
    if let Some(Value::Array(entries)) = map.get("allOf")
        && !entries
            .iter()
            .all(|entry| allows_null(entry, root, chain, budget))
    {
        return false;
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = map.get(key)
            && !branches
                .iter()
                .any(|branch| allows_null(branch, root, chain, budget))
        {
            return false;
        }
    }
    match ref_target(map, root, chain) {
        Some(Some(target)) => allows_null(target, root, chain + 1, budget),
        Some(None) | None => true,
    }
}

/// The schemas a value must satisfy: every entry of `schemas`, plus one
/// alternative from each of `unions` (when it is not known which one applies).
#[derive(Default)]
struct Constraint<'a> {
    schemas: Vec<&'a Value>,
    unions: Vec<Vec<Constraint<'a>>>,
}

impl Constraint<'_> {
    fn is_empty(&self) -> bool {
        self.schemas.is_empty() && self.unions.is_empty()
    }
}

/// A [`Constraint`] with references, `allOf` and decidable unions flattened
/// into the object maps that apply to one concrete value.
#[derive(Default)]
struct Resolved<'a> {
    maps: Vec<&'a Map<String, Value>>,
    unions: Vec<Vec<Resolved<'a>>>,
}

#[derive(Clone, Copy)]
enum ChildKey<'k> {
    Field(&'k str),
    Index(usize),
}

fn strip_value<'a>(value: &mut Value, constraint: &Constraint<'a>, root: &'a Value) {
    let Some(kind) = ValueKind::of(value) else {
        return;
    };
    let mut expansion = Expansion {
        root,
        budget: EXPANSION_BUDGET,
        complete: true,
    };
    let resolved = expansion.constraint(constraint, kind, value);
    if !expansion.complete {
        return;
    }
    match value {
        Value::Object(fields) => {
            let mut null_budget = ALLOWS_NULL_BUDGET;
            fields.retain(|name, field| {
                !(field.is_null()
                    && null_field_is_injected(&resolved, name, root, &mut null_budget))
            });
            for (name, field) in fields.iter_mut() {
                let child = child_constraint(&resolved, ChildKey::Field(name));
                if !child.is_empty() {
                    strip_value(field, &child, root);
                }
            }
        }
        Value::Array(elements) => {
            for (index, element) in elements.iter_mut().enumerate() {
                let child = child_constraint(&resolved, ChildKey::Index(index));
                if !child.is_empty() {
                    strip_value(element, &child, root);
                }
            }
        }
        _ => {}
    }
}

fn null_field_is_injected(
    resolved: &Resolved<'_>,
    name: &str,
    root: &Value,
    budget: &mut usize,
) -> bool {
    !field_required(resolved, name) && field_rejects_null(resolved, name, root, budget)
}

fn field_required(resolved: &Resolved<'_>, name: &str) -> bool {
    resolved
        .maps
        .iter()
        .any(|map| required_names(map).contains(&name))
        || resolved
            .unions
            .iter()
            .any(|alternatives| alternatives.iter().any(|alt| field_required(alt, name)))
}

fn field_rejects_null(
    resolved: &Resolved<'_>,
    name: &str,
    root: &Value,
    budget: &mut usize,
) -> bool {
    resolved.maps.iter().any(|map| {
        map.get("properties")
            .and_then(|properties| properties.get(name))
            .is_some_and(|property| !allows_null(property, root, 0, budget))
    }) || resolved.unions.iter().any(|alternatives| {
        alternatives
            .iter()
            .all(|alt| field_rejects_null(alt, name, root, budget))
    })
}

fn child_constraint<'a>(resolved: &Resolved<'a>, key: ChildKey<'_>) -> Constraint<'a> {
    Constraint {
        schemas: resolved
            .maps
            .iter()
            .filter_map(|map| child_schema(map, key))
            .collect(),
        unions: resolved
            .unions
            .iter()
            .map(|alternatives| {
                alternatives
                    .iter()
                    .map(|alt| child_constraint(alt, key))
                    .collect()
            })
            .collect(),
    }
}

fn child_schema<'a>(map: &'a Map<String, Value>, key: ChildKey<'_>) -> Option<&'a Value> {
    match key {
        ChildKey::Field(name) => map
            .get("properties")
            .and_then(|properties| properties.get(name))
            .or_else(|| {
                map.get("additionalProperties")
                    .filter(|extra| extra.is_object())
            }),
        ChildKey::Index(index) => match map.get("items") {
            Some(Value::Array(tuple)) => tuple.get(index),
            items => items,
        },
    }
}

/// Flattens the schemas that apply to one value. Anything it cannot decide
/// (an unresolvable reference, no matching union branch, an exhausted budget,
/// a keyword the walk does not model) clears `complete`, and the caller then
/// leaves the value untouched.
struct Expansion<'a> {
    root: &'a Value,
    budget: usize,
    complete: bool,
}

impl<'a> Expansion<'a> {
    fn constraint(
        &mut self,
        constraint: &Constraint<'a>,
        kind: ValueKind,
        value: &Value,
    ) -> Resolved<'a> {
        let mut out = Resolved::default();
        for schema in &constraint.schemas {
            self.schema(schema, 0, kind, value, &mut out);
        }
        for union in &constraint.unions {
            let alternatives = union
                .iter()
                .map(|alt| self.constraint(alt, kind, value))
                .collect();
            out.unions.push(alternatives);
        }
        out
    }

    fn schema(
        &mut self,
        schema: &'a Value,
        chain: usize,
        kind: ValueKind,
        value: &Value,
        out: &mut Resolved<'a>,
    ) {
        if self.budget == 0 {
            self.complete = false;
            return;
        }
        self.budget -= 1;
        let Value::Object(map) = schema else {
            return;
        };
        if UNMODELLED_KEYWORDS.iter().any(|key| map.contains_key(*key)) {
            self.complete = false;
            return;
        }
        out.maps.push(map);
        if let Some(Value::Array(entries)) = map.get("allOf") {
            for entry in entries {
                self.schema(entry, chain, kind, value, out);
            }
        }
        for key in ["anyOf", "oneOf"] {
            let Some(Value::Array(branches)) = map.get(key) else {
                continue;
            };
            match self.candidates(branches, chain, kind, value).as_slice() {
                [] => self.complete = false,
                [only] => self.schema(only, chain, kind, value, out),
                several => {
                    let alternatives = several
                        .iter()
                        .map(|branch| {
                            let mut alternative = Resolved::default();
                            self.schema(branch, chain, kind, value, &mut alternative);
                            alternative
                        })
                        .collect();
                    out.unions.push(alternatives);
                }
            }
        }
        match ref_target(map, self.root, chain) {
            Some(Some(target)) => self.schema(target, chain + 1, kind, value, out),
            Some(None) => {}
            None => self.complete = false,
        }
    }

    /// Branches that may describe `value`: the right shape, and no
    /// discriminator property (`const` or single-value `enum`) contradicting it.
    fn candidates(
        &self,
        branches: &'a [Value],
        chain: usize,
        kind: ValueKind,
        value: &Value,
    ) -> Vec<&'a Value> {
        branches
            .iter()
            .filter(|branch| self.may_describe(branch, chain, kind, value))
            .collect()
    }

    fn may_describe(&self, branch: &Value, chain: usize, kind: ValueKind, value: &Value) -> bool {
        let map = match branch {
            Value::Object(map) => map,
            Value::Bool(accepts_anything) => return *accepts_anything,
            _ => return false,
        };
        if !branch_shape_matches(map, kind) || !discriminators_match(map, value) {
            return false;
        }
        match ref_target(map, self.root, chain) {
            Some(Some(target)) => self.may_describe(target, chain + 1, kind, value),
            Some(None) | None => true,
        }
    }
}

fn branch_shape_matches(map: &Map<String, Value>, kind: ValueKind) -> bool {
    match branch_kind(map) {
        BranchKind::NullOnly => false,
        BranchKind::Typed(kinds) => kinds.contains(&kind.name()),
        BranchKind::Unknown => true,
    }
}

fn discriminators_match(map: &Map<String, Value>, value: &Value) -> bool {
    let Some(Value::Object(properties)) = map.get("properties") else {
        return true;
    };
    properties.iter().all(
        |(name, property)| match (discriminator(property), value.get(name)) {
            (Some(_), Some(Value::Null)) => true,
            (Some(expected), Some(actual)) => expected == actual,
            _ => true,
        },
    )
}

fn discriminator(property: &Value) -> Option<&Value> {
    if let Some(expected) = property.get("const") {
        return Some(expected);
    }
    match property.get("enum") {
        Some(Value::Array(variants)) if variants.len() == 1 => variants.first(),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Object,
    Array,
}

impl ValueKind {
    fn of(value: &Value) -> Option<Self> {
        match value {
            Value::Object(_) => Some(Self::Object),
            Value::Array(_) => Some(Self::Array),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
        }
    }
}

enum BranchKind<'a> {
    NullOnly,
    Typed(Vec<&'a str>),
    Unknown,
}

fn branch_kind(map: &Map<String, Value>) -> BranchKind<'_> {
    if map.get("const").is_some_and(Value::is_null) {
        return BranchKind::NullOnly;
    }
    let types: Vec<&str> = match map.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if types.is_empty() {
        BranchKind::Unknown
    } else if types.iter().all(|t| *t == "null") {
        BranchKind::NullOnly
    } else {
        BranchKind::Typed(types)
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
    fn openai_strict_wraps_optional_typed_const_in_any_of() {
        let schema = json!({
            "type": "object",
            "properties": {
                "fixed": { "type": "string", "const": "x" },
                "fixed_null": { "const": null }
            }
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);
        let properties = &converted["properties"];

        assert_eq!(
            properties["fixed"],
            json!({ "anyOf": [{ "type": "string", "const": "x" }, { "type": "null" }] })
        );
        assert_eq!(properties["fixed_null"], json!({ "const": null }));
    }

    #[test]
    fn openai_strict_appends_null_branch_to_existing_any_of() {
        let schema = json!({
            "type": "object",
            "properties": {
                "either": { "anyOf": [{ "type": "string" }, { "type": "integer" }] },
                "const_null": { "anyOf": [{ "type": "string" }, { "const": null }] },
                "enum_null": { "anyOf": [{ "type": "string" }, { "enum": [1, null] }] }
            }
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);
        let properties = &converted["properties"];

        assert_eq!(
            properties["either"],
            json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }, { "type": "null" }] })
        );
        assert_eq!(
            properties["const_null"],
            json!({ "anyOf": [{ "type": "string" }, { "const": null }] })
        );
        assert_eq!(
            properties["enum_null"],
            json!({ "anyOf": [{ "type": "string" }, { "enum": [1, null] }] })
        );
    }

    #[test]
    fn one_of_beside_any_of_moves_into_description() {
        let schema = json!({
            "anyOf": [{ "type": "string" }],
            "oneOf": [{ "type": "integer" }]
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(
            converted,
            json!({
                "anyOf": [{ "type": "string" }],
                "description": "{oneOf: [{\"type\":\"integer\"}]}"
            })
        );
    }

    #[test]
    fn empty_description_is_treated_as_absent() {
        let schema = json!({ "type": "integer", "description": "", "minimum": 1 });

        let converted = convert_schema(&schema, SchemaDialect::Anthropic);

        assert_eq!(converted["description"], json!("{minimum: 1}"));
    }

    #[test]
    fn openai_strict_property_less_object_requires_nothing() {
        let converted = convert_schema(&json!({ "type": "object" }), SchemaDialect::OpenAiStrict);

        assert_eq!(converted, json!({ "type": "object", "required": [] }));
    }

    #[test]
    fn property_less_objects_stay_open() {
        let open_objects = [
            json!({ "type": "object" }),
            json!({ "type": "object", "properties": {} }),
        ];

        for object in open_objects {
            let nested = json!({ "type": "array", "items": object.clone() });
            for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
                let top = convert_schema(&object, dialect);
                let nested = convert_schema(&nested, dialect);

                assert!(
                    top.get("additionalProperties").is_none(),
                    "{dialect:?} {object}"
                );
                assert!(
                    nested["items"].get("additionalProperties").is_none(),
                    "{dialect:?} {object}"
                );
                let expected_required = match dialect {
                    SchemaDialect::OpenAiStrict => Some(&json!([])),
                    _ => None,
                };
                assert_eq!(
                    top.get("required"),
                    expected_required,
                    "{dialect:?} {object}"
                );
                assert_eq!(
                    nested["items"].get("required"),
                    expected_required,
                    "{dialect:?} {object}"
                );
            }
        }
    }

    #[test]
    fn objects_with_properties_are_closed() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": { "a": { "type": "string" } }
            }
        });

        for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
            let converted = convert_schema(&schema, dialect);

            assert_eq!(
                converted["items"]["additionalProperties"],
                json!(false),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn explicit_additional_properties_on_property_less_object_is_preserved() {
        let schema = json!({ "type": "object", "additionalProperties": true });

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
    fn strip_injected_nulls_keeps_nulls_inside_property_less_objects() {
        let schema = json!({ "type": "array", "items": { "type": "object" } });
        let mut value = json!([{ "note": null }]);

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!([{ "note": null }]));
    }

    #[test]
    fn schema_valued_additional_properties_is_converted() {
        let schema = json!({
            "type": "object",
            "additionalProperties": { "type": "string", "minLength": 1 }
        });

        for dialect in [SchemaDialect::OpenAiStrict, SchemaDialect::Anthropic] {
            let converted = convert_schema(&schema, dialect);
            assert_eq!(
                converted["additionalProperties"],
                json!({ "type": "string", "description": "{minLength: 1}" }),
                "{dialect:?}"
            );
        }
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
    fn strip_injected_nulls_follows_nullable_any_of_branch() {
        let schema = json!({
            "type": "object",
            "properties": {
                "inner": { "anyOf": [{ "$ref": "#/$defs/x" }, { "type": "null" }] }
            },
            "$defs": {
                "x": {
                    "type": "object",
                    "properties": { "note": { "type": "string" } }
                }
            }
        });
        let mut present = json!({ "inner": { "note": null } });
        let mut absent = json!({ "inner": null });

        strip_injected_nulls(&mut present, &schema);
        strip_injected_nulls(&mut absent, &schema);

        assert_eq!(present, json!({ "inner": {} }));
        assert_eq!(absent, json!({ "inner": null }));
    }

    #[test]
    fn strip_injected_nulls_picks_one_of_branch_by_shape() {
        let schema = json!({
            "oneOf": [
                { "type": "array", "items": { "$ref": "#/$defs/x" } },
                { "$ref": "#/$defs/x" }
            ],
            "$defs": {
                "x": {
                    "type": "object",
                    "properties": { "note": { "type": "string" } }
                }
            }
        });
        let mut object = json!({ "note": null });
        let mut array = json!([{ "note": null }, { "note": "kept" }]);

        strip_injected_nulls(&mut object, &schema);
        strip_injected_nulls(&mut array, &schema);

        assert_eq!(object, json!({}));
        assert_eq!(array, json!([{}, { "note": "kept" }]));
    }

    #[test]
    fn strip_injected_nulls_merges_all_of_entries() {
        let schema = json!({
            "allOf": [
                {
                    "type": "object",
                    "properties": { "a": { "type": "string" } },
                    "required": ["a"]
                },
                { "$ref": "#/$defs/extra" }
            ],
            "$defs": {
                "extra": {
                    "type": "object",
                    "properties": { "b": { "type": "string" } }
                }
            }
        });
        let mut value = json!({ "a": null, "b": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "a": null }));
    }

    #[test]
    fn strip_injected_nulls_applies_additional_properties_schema() {
        let schema = json!({
            "type": "object",
            "properties": { "listed": { "type": "object" } },
            "additionalProperties": {
                "type": "object",
                "properties": { "note": { "type": "string" } }
            }
        });
        let mut value = json!({
            "listed": { "note": null },
            "extra": { "note": null, "other": 1 }
        });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(
            value,
            json!({ "listed": { "note": null }, "extra": { "other": 1 } })
        );
    }

    #[test]
    fn strip_injected_nulls_keeps_nulls_for_permissive_schemas() {
        let schema = json!({
            "type": "object",
            "properties": {
                "const_null": { "const": null },
                "anything": true,
                "unconstrained": {}
            }
        });
        let mut value = json!({ "const_null": null, "anything": null, "unconstrained": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(
            value,
            json!({ "const_null": null, "anything": null, "unconstrained": null })
        );
    }

    #[test]
    fn strip_injected_nulls_walks_tuple_items() {
        let schema = json!({
            "type": "array",
            "items": [
                { "type": "object", "properties": { "a": { "type": "string" } } },
                { "type": "object", "properties": { "a": { "type": ["string", "null"] } } }
            ]
        });
        let mut value = json!([{ "a": null }, { "a": null }]);

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!([{}, { "a": null }]));
    }

    #[test]
    fn strip_injected_nulls_keeps_nulls_behind_non_local_refs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "remote": { "$ref": "https://example.com/schema.json" },
                "pointer": { "$ref": "#/properties/remote" }
            }
        });
        let mut value = json!({ "remote": null, "pointer": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "remote": null, "pointer": null }));
    }

    #[test]
    fn strip_injected_nulls_follows_definitions_refs() {
        let schema = json!({
            "type": "object",
            "properties": { "inner": { "$ref": "#/definitions/x" } },
            "required": ["inner"],
            "definitions": {
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
    fn strip_injected_nulls_evaluates_all_of_and_not() {
        let schema = json!({
            "type": "object",
            "properties": {
                "nullable_all": { "allOf": [{ "type": ["string", "null"] }] },
                "strict_all": { "allOf": [{ "type": "string" }, { "description": "x" }] },
                "negated": { "not": { "type": "string" } }
            }
        });
        let mut value = json!({ "nullable_all": null, "strict_all": null, "negated": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "nullable_all": null, "negated": null }));
    }

    #[test]
    fn strip_injected_nulls_merges_nested_all_of_properties() {
        let schema = json!({
            "allOf": [
                { "properties": { "x": { "type": "object", "required": ["a"] } } },
                { "properties": { "x": { "properties": { "a": { "type": "string" } } } } }
            ]
        });
        let mut value = json!({ "x": { "a": null } });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "x": { "a": null } }));
    }

    #[test]
    fn strip_injected_nulls_narrows_union_by_discriminator() {
        let schema = json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": { "kind": { "const": "a" }, "x": { "type": "string" } },
                    "required": ["kind"]
                },
                {
                    "type": "object",
                    "properties": { "kind": { "enum": ["b"] }, "y": { "type": "string" } },
                    "required": ["kind"]
                }
            ]
        });
        let mut value = json!({ "kind": "b", "y": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "kind": "b" }));
    }

    #[test]
    fn strip_injected_nulls_requires_agreement_across_ambiguous_branches() {
        let schema = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": { "note": { "type": "string" }, "other": { "type": "string" } }
                },
                {
                    "type": "object",
                    "properties": {
                        "note": { "type": ["string", "null"] },
                        "other": { "type": "integer" }
                    }
                }
            ]
        });
        let mut value = json!({ "note": null, "other": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "note": null }));
    }

    #[test]
    fn strip_injected_nulls_reaches_the_bottom_of_a_deep_recursive_tree() {
        let schema = json!({
            "$ref": "#/definitions/node",
            "definitions": {
                "node": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "child": { "$ref": "#/definitions/node" }
                    }
                }
            }
        });
        let nest = |leaf: Value| {
            (0..40).fold(
                leaf,
                |inner, depth| json!({ "label": format!("level {depth}"), "child": inner }),
            )
        };
        let mut value = nest(json!({ "label": null }));

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, nest(json!({})));
    }

    #[test]
    fn strip_injected_nulls_bounds_branching_ref_cycles() {
        let schema = json!({
            "type": "object",
            "properties": {
                "scalar": { "$ref": "#/$defs/a" },
                "nested": { "$ref": "#/$defs/a" }
            },
            "$defs": {
                "a": {
                    "anyOf": [
                        { "$ref": "#/$defs/a" },
                        { "$ref": "#/$defs/a" },
                        { "$ref": "#/$defs/a" },
                        { "$ref": "#/$defs/a" }
                    ]
                }
            }
        });
        let mut value = json!({ "scalar": null, "nested": { "inner": null } });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(
            value,
            json!({ "scalar": null, "nested": { "inner": null } })
        );
    }

    #[test]
    fn openai_strict_leaves_typeless_enum_with_null_unwrapped() {
        let schema = json!({
            "type": "object",
            "properties": { "choice": { "enum": ["a", null] } }
        });

        let converted = convert_schema(&schema, SchemaDialect::OpenAiStrict);

        assert_eq!(
            converted["properties"]["choice"],
            json!({ "enum": ["a", null] })
        );
    }

    #[test]
    fn strip_injected_nulls_null_discriminator_does_not_exclude_a_branch() {
        let schema = json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "a" },
                        "x": { "type": ["string", "null"] }
                    }
                },
                { "type": "object", "properties": { "x": { "type": "string" } } }
            ]
        });
        let mut value = json!({ "kind": null, "x": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "kind": null, "x": null }));
    }

    #[test]
    fn strip_injected_nulls_merges_ref_sibling_required() {
        let schema = json!({
            "type": "object",
            "properties": { "inner": { "$ref": "#/$defs/x", "required": ["note"] } },
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

        assert_eq!(value, json!({ "inner": { "note": null } }));
    }

    #[test]
    fn strip_injected_nulls_keeps_nulls_under_prefix_items() {
        let element = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        let schema = json!({ "type": "array", "prefixItems": [element], "items": element });
        let mut value = json!([{ "a": null }, { "a": null }]);

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!([{ "a": null }, { "a": null }]));
    }

    #[test]
    fn strip_injected_nulls_keeps_nulls_under_pattern_properties() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "patternProperties": { "^a$": { "type": ["string", "null"] } }
        });
        let mut value = json!({ "a": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "a": null }));
    }

    #[test]
    fn strip_injected_nulls_treats_untyped_branches_as_any_kind() {
        let schema = json!({
            "oneOf": [
                { "items": { "type": "string" } },
                { "type": "object", "properties": { "a": { "type": "string" } } }
            ]
        });
        let mut value = json!({ "a": null });

        strip_injected_nulls(&mut value, &schema);

        assert_eq!(value, json!({ "a": null }));
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
