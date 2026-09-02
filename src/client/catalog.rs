use super::model::ModelData;
use super::oauth::OAuthConfig;

use fancy_regex::Regex;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

pub const QUIRKS_YAML: &str = include_str!("../../quirks.yaml");

pub static ALL_QUIRKS: LazyLock<Vec<ProviderQuirks>> =
    LazyLock::new(|| serde_yaml::from_str(QUIRKS_YAML).expect("quirks.yaml must be valid"));

/// Hand-owned catalog overrides for a single provider: oauth configuration,
/// curation excludes, field overlays for API-sourced models, suffixed model
/// variants, and fully hand-owned model entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuirks {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<QuirkRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<QuirkVariant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelData>,
}

/// A field overlay applied to every API-sourced model whose name matches the
/// glob in `match` (`*` = any run of characters, `?` = exactly one). Only
/// explicitly present fields are written; absent fields leave the model
/// untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuirkRule {
    pub r#match: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<isize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_max_tokens: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_function_calling: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_system_message: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<String>,
}

/// Clones every model whose name matches the glob in `match`, appends `suffix`
/// to the clone's name, and overlays any explicitly present fields onto the
/// clone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuirkVariant {
    pub r#match: String,
    pub suffix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<isize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_max_tokens: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_function_calling: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_system_message: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<String>,
}

/// Applies every rule whose glob matches `data.name`, in file order; later
/// rules overwrite earlier ones on the same field.
#[allow(dead_code)]
pub fn apply_rules(provider_quirks: &ProviderQuirks, data: &mut ModelData) {
    for rule in &provider_quirks.rules {
        if !glob_matches(&rule.r#match, &data.name) {
            continue;
        }
        if let Some(real_name) = &rule.real_name {
            data.real_name = Some(real_name.clone());
        }
        if let Some(max_input_tokens) = rule.max_input_tokens {
            data.max_input_tokens = Some(max_input_tokens);
        }
        if let Some(max_output_tokens) = rule.max_output_tokens {
            data.max_output_tokens = Some(max_output_tokens);
        }
        if let Some(input_price) = rule.input_price {
            data.input_price = Some(input_price);
        }
        if let Some(output_price) = rule.output_price {
            data.output_price = Some(output_price);
        }
        if let Some(patch) = &rule.patch {
            data.patch = Some(patch.clone());
        }
        if let Some(require_max_tokens) = rule.require_max_tokens {
            data.require_max_tokens = require_max_tokens;
        }
        if let Some(supports_vision) = rule.supports_vision {
            data.supports_vision = supports_vision;
        }
        if let Some(supports_function_calling) = rule.supports_function_calling {
            data.supports_function_calling = supports_function_calling;
        }
        if let Some(reasoning_levels) = &rule.reasoning_levels {
            data.reasoning_levels = reasoning_levels.clone();
        }
        if let Some(default_reasoning_effort) = &rule.default_reasoning_effort {
            data.default_reasoning_effort = Some(default_reasoning_effort.clone());
        }
        if let Some(no_stream) = rule.no_stream {
            data.no_stream = no_stream;
        }
        if let Some(no_system_message) = rule.no_system_message {
            data.no_system_message = no_system_message;
        }
        if let Some(system_prompt_prefix) = &rule.system_prompt_prefix {
            data.system_prompt_prefix = Some(system_prompt_prefix.clone());
        }
    }
}

/// Finds the quirks for a client: an exact provider match wins, otherwise a
/// client whose name starts with the provider name (openai-compatible clients
/// are conventionally named after the provider they wrap).
#[allow(dead_code)]
pub fn quirks_for(client_name: &str) -> Option<&'static ProviderQuirks> {
    ALL_QUIRKS
        .iter()
        .find(|quirks| quirks.provider == client_name)
        .or_else(|| {
            ALL_QUIRKS
                .iter()
                .find(|quirks| client_name.starts_with(&quirks.provider))
        })
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    compile_glob(pattern).is_match(name).unwrap_or(false)
}

/// Translates a glob pattern (`*` = any run of characters, `?` = exactly one)
/// into an anchored regex. Patterns that fail to compile match nothing.
fn compile_glob(pattern: &str) -> Regex {
    let translated = format!(
        "^{}$",
        fancy_regex::escape(pattern)
            .replace("\\*", ".*")
            .replace("\\?", ".")
    );
    Regex::new(&translated).unwrap_or_else(|error| {
        warn!("Invalid model quirk pattern '{pattern}': {error}. It will match nothing.");
        Regex::new("(?!)").expect("'(?!)' is a valid never-matching regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN_PROVIDERS: [&str; 24] = [
        "openai",
        "gemini",
        "claude",
        "mistral",
        "ai21",
        "cohere",
        "xai",
        "perplexity",
        "groq",
        "vertexai",
        "bedrock",
        "cloudflare",
        "ernie",
        "qianwen",
        "hunyuan",
        "moonshot",
        "deepseek",
        "zhipuai",
        "minimax",
        "openrouter",
        "github",
        "deepinfra",
        "jina",
        "voyageai",
    ];

    #[test]
    fn quirks_yaml_parses() {
        assert!(!ALL_QUIRKS.is_empty());
        for quirks in ALL_QUIRKS.iter() {
            assert!(
                KNOWN_PROVIDERS.contains(&quirks.provider.as_str()),
                "unknown provider '{}' in quirks.yaml",
                quirks.provider
            );
        }
    }

    #[test]
    fn quirks_models_no_dropped_keys() {
        let skipped_when_default = [
            "type",
            "require_max_tokens",
            "supports_vision",
            "supports_function_calling",
            "reasoning_levels",
            "no_stream",
            "no_system_message",
        ];
        let raw: serde_yaml::Value = serde_yaml::from_str(QUIRKS_YAML).unwrap();
        for provider in raw.as_sequence().unwrap() {
            let Some(models) = provider
                .get("models")
                .and_then(|models| models.as_sequence())
            else {
                continue;
            };
            for entry in models {
                let parsed: ModelData = serde_yaml::from_value(entry.clone()).unwrap();
                let reserialized = serde_yaml::to_value(&parsed).unwrap();
                let reserialized_keys: Vec<&str> = reserialized
                    .as_mapping()
                    .unwrap()
                    .keys()
                    .map(|key| key.as_str().unwrap())
                    .collect();
                for key in entry.as_mapping().unwrap().keys() {
                    let key = key.as_str().unwrap();
                    assert!(
                        reserialized_keys.contains(&key) || skipped_when_default.contains(&key),
                        "key '{key}' on model '{}' was dropped by ModelData",
                        parsed.name
                    );
                }
            }
        }
    }

    #[test]
    fn rules_apply_in_order() {
        let quirks: ProviderQuirks = serde_yaml::from_str(
            r#"
provider: openai
rules:
  - match: "model-*"
    default_reasoning_effort: low
    require_max_tokens: true
  - match: "model-a"
    default_reasoning_effort: high
"#,
        )
        .unwrap();

        let mut matched_by_both = ModelData::new("model-a");
        apply_rules(&quirks, &mut matched_by_both);
        assert_eq!(
            matched_by_both.default_reasoning_effort.as_deref(),
            Some("high")
        );
        assert!(matched_by_both.require_max_tokens);

        let mut matched_by_glob = ModelData::new("model-b");
        apply_rules(&quirks, &mut matched_by_glob);
        assert_eq!(
            matched_by_glob.default_reasoning_effort.as_deref(),
            Some("low")
        );

        let mut unmatched = ModelData::new("other");
        apply_rules(&quirks, &mut unmatched);
        assert_eq!(unmatched.default_reasoning_effort, None);
        assert!(!unmatched.require_max_tokens);
    }

    #[test]
    fn glob_semantics() {
        assert!(glob_matches("claude-*", "claude-opus-4-8"));
        assert!(!glob_matches("claude-*", "gpt-4"));
        assert!(glob_matches("claude-*", "claude-"));
        assert!(glob_matches("gpt-?", "gpt-4"));
        assert!(!glob_matches("gpt-?", "gpt-45"));
        assert!(!glob_matches("gpt-?", "gpt-"));
        assert!(glob_matches("o3", "o3"));
        assert!(!glob_matches("o3", "o3-mini"));
    }
}
