use super::model::{ModelData, ProviderModels};
use super::oauth::OAuthConfig;

use crate::utils::fetch;

use anyhow::{Result, bail};
use fancy_regex::Regex;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;

pub const QUIRKS_YAML: &str = include_str!("../../quirks.yaml");

#[allow(dead_code)]
pub static ALL_QUIRKS: LazyLock<Vec<ProviderQuirks>> =
    LazyLock::new(|| serde_yaml::from_str(QUIRKS_YAML).expect("quirks.yaml must be valid"));

const MODELSDEV_API_URL: &str = "https://models.dev/api.json";
const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1/models";

/// Upstream ids for one coyote provider. The table order is the fixed output
/// order of the built catalog. Providers with neither upstream id are fully
/// quirks-owned; `openrouter` is special-cased in the merge: its models come
/// from the OpenRouter list natively instead of via a prefix join.
struct SourceIds {
    provider: &'static str,
    modelsdev: Option<&'static str>,
    openrouter: Option<&'static str>,
}

const PROVIDER_SOURCES: [SourceIds; 24] = [
    SourceIds {
        provider: "openai",
        modelsdev: Some("openai"),
        openrouter: Some("openai"),
    },
    SourceIds {
        provider: "gemini",
        modelsdev: Some("google"),
        openrouter: Some("google"),
    },
    SourceIds {
        provider: "claude",
        modelsdev: Some("anthropic"),
        openrouter: Some("anthropic"),
    },
    SourceIds {
        provider: "mistral",
        modelsdev: Some("mistral"),
        openrouter: Some("mistralai"),
    },
    SourceIds {
        provider: "ai21",
        modelsdev: None,
        openrouter: None,
    },
    SourceIds {
        provider: "cohere",
        modelsdev: Some("cohere"),
        openrouter: Some("cohere"),
    },
    SourceIds {
        provider: "xai",
        modelsdev: Some("xai"),
        openrouter: Some("x-ai"),
    },
    SourceIds {
        provider: "perplexity",
        modelsdev: Some("perplexity"),
        openrouter: Some("perplexity"),
    },
    SourceIds {
        provider: "groq",
        modelsdev: Some("groq"),
        openrouter: Some("groq"),
    },
    SourceIds {
        provider: "vertexai",
        modelsdev: Some("google-vertex"),
        openrouter: Some("google"),
    },
    SourceIds {
        provider: "bedrock",
        modelsdev: Some("amazon-bedrock"),
        openrouter: Some("amazon"),
    },
    SourceIds {
        provider: "cloudflare",
        modelsdev: Some("cloudflare-workers-ai"),
        openrouter: None,
    },
    SourceIds {
        provider: "ernie",
        modelsdev: None,
        openrouter: None,
    },
    SourceIds {
        provider: "qianwen",
        modelsdev: Some("alibaba"),
        openrouter: Some("qwen"),
    },
    SourceIds {
        provider: "hunyuan",
        modelsdev: None,
        openrouter: None,
    },
    SourceIds {
        provider: "moonshot",
        modelsdev: Some("moonshotai"),
        openrouter: Some("moonshotai"),
    },
    SourceIds {
        provider: "deepseek",
        modelsdev: Some("deepseek"),
        openrouter: Some("deepseek"),
    },
    SourceIds {
        provider: "zhipuai",
        modelsdev: Some("zhipuai"),
        openrouter: Some("zhipuai"),
    },
    SourceIds {
        provider: "minimax",
        modelsdev: Some("minimax"),
        openrouter: Some("minimax"),
    },
    SourceIds {
        provider: "openrouter",
        modelsdev: None,
        openrouter: None,
    },
    SourceIds {
        provider: "github",
        modelsdev: Some("github-copilot"),
        openrouter: Some("github"),
    },
    SourceIds {
        provider: "deepinfra",
        modelsdev: Some("deepinfra"),
        openrouter: None,
    },
    SourceIds {
        provider: "jina",
        modelsdev: None,
        openrouter: None,
    },
    SourceIds {
        provider: "voyageai",
        modelsdev: None,
        openrouter: None,
    },
];

/// Raw payloads from the two upstream catalogs. Each source is independently
/// fallible; `None` means that fetch failed and the merge degrades gracefully.
pub struct FetchedSources {
    pub modelsdev: Option<String>,
    pub openrouter: Option<String>,
}

/// Fetches both upstream catalogs concurrently. A failed fetch is logged and
/// surfaces as `None` rather than an error.
#[allow(dead_code)]
pub async fn fetch_sources() -> FetchedSources {
    let (modelsdev, openrouter) = tokio::join!(fetch(MODELSDEV_API_URL), fetch(OPENROUTER_API_URL));
    FetchedSources {
        modelsdev: modelsdev
            .inspect_err(|err| warn!("Failed to fetch {MODELSDEV_API_URL}: {err}"))
            .ok(),
        openrouter: openrouter
            .inspect_err(|err| warn!("Failed to fetch {OPENROUTER_API_URL}: {err}"))
            .ok(),
    }
}

/// An API-sourced model plus the release date used for ordering. The date is
/// deliberately kept out of [`ModelData`] and never serialized.
struct SourcedModel {
    data: ModelData,
    release_date: Option<String>,
}

fn clamp_input_tokens(value: u64) -> Option<usize> {
    (1..=50_000_000).contains(&value).then_some(value as usize)
}

fn clamp_output_tokens(value: i64) -> Option<isize> {
    (1..=10_000_000).contains(&value).then_some(value as isize)
}

fn clamp_price(value: f64) -> Option<f64> {
    (0.0..=10_000.0).contains(&value).then_some(value)
}

/// Extracts the models of one models.dev provider. Tolerant by design:
/// missing or malformed fields are skipped while the model is kept, and
/// out-of-range numbers are dropped field-wise.
fn modelsdev_provider_models(api: &Value, provider_id: &str) -> Vec<SourcedModel> {
    let Some(models) = api[provider_id]["models"].as_object() else {
        return Vec::new();
    };
    models
        .iter()
        .map(|(name, entry)| {
            let mut data = ModelData::new(name);
            data.max_input_tokens = entry["limit"]["context"]
                .as_u64()
                .and_then(clamp_input_tokens);
            data.max_output_tokens = entry["limit"]["output"]
                .as_i64()
                .and_then(clamp_output_tokens);
            data.input_price = entry["cost"]["input"].as_f64().and_then(clamp_price);
            data.output_price = entry["cost"]["output"].as_f64().and_then(clamp_price);
            data.supports_vision = entry["modalities"]["input"]
                .as_array()
                .is_some_and(|modes| modes.iter().any(|mode| mode.as_str() == Some("image")));
            data.supports_function_calling = entry["tool_call"].as_bool().unwrap_or(false);
            data.reasoning_levels = entry["reasoning_options"]
                .as_array()
                .and_then(|options| {
                    options
                        .iter()
                        .find(|option| option["type"].as_str() == Some("effort"))
                })
                .and_then(|option| option["values"].as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let release_date = entry["release_date"].as_str().map(str::to_string);
            SourcedModel { data, release_date }
        })
        .collect()
}

/// OpenRouter prices are USD per token; the catalog stores USD per million
/// tokens, rounded to 4 decimals.
fn openrouter_price(value: &Value) -> Option<f64> {
    let per_token: f64 = value.as_str()?.parse().ok()?;
    clamp_price((per_token * 1_000_000.0 * 10_000.0).round() / 10_000.0)
}

/// Maps one OpenRouter list entry onto [`ModelData`], with the same field-wise
/// tolerance and clamping as the models.dev adapter.
fn openrouter_model_data(name: &str, entry: &Value) -> ModelData {
    let mut data = ModelData::new(name);
    data.max_input_tokens = entry["context_length"]
        .as_u64()
        .and_then(clamp_input_tokens);
    data.max_output_tokens = entry["top_provider"]["max_completion_tokens"]
        .as_i64()
        .and_then(clamp_output_tokens);
    data.input_price = openrouter_price(&entry["pricing"]["prompt"]);
    data.output_price = openrouter_price(&entry["pricing"]["completion"]);
    data.supports_vision = entry["architecture"]["modality"]
        .as_str()
        .is_some_and(|modality| modality.contains("image"));
    data.supports_function_calling = entry["supported_parameters"]
        .as_array()
        .is_some_and(|params| params.iter().any(|param| param.as_str() == Some("tools")));
    data
}

fn openrouter_index(payload: &Value) -> HashMap<&str, &Value> {
    payload["data"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| Some((entry["id"].as_str()?, entry)))
                .collect()
        })
        .unwrap_or_default()
}

/// Every OpenRouter list entry as a model of the `openrouter` provider, named
/// by its full id. OpenRouter publishes no release date, so ordering falls
/// back to name-ascending.
fn openrouter_native_models(payload: &Value) -> Vec<SourcedModel> {
    payload["data"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let id = entry["id"].as_str()?;
                    Some(SourcedModel {
                        data: openrouter_model_data(id, entry),
                        release_date: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Fills fields that models.dev left unset on the same model: OpenRouter data
/// never overrides a present models.dev value.
fn fill_from_openrouter(data: &mut ModelData, entry: &Value) {
    let filler = openrouter_model_data(&data.name, entry);
    if data.max_input_tokens.is_none() {
        data.max_input_tokens = filler.max_input_tokens;
    }
    if data.max_output_tokens.is_none() {
        data.max_output_tokens = filler.max_output_tokens;
    }
    if data.input_price.is_none() {
        data.input_price = filler.input_price;
    }
    if data.output_price.is_none() {
        data.output_price = filler.output_price;
    }
    if !data.supports_vision {
        data.supports_vision = filler.supports_vision;
    }
    if !data.supports_function_calling {
        data.supports_function_calling = filler.supports_function_calling;
    }
}

/// Outcome of a catalog merge. `degraded` lists API-mapped providers that fell
/// back to their hand-owned quirks models because API data was unavailable or
/// empty; `failed` lists providers that emitted nothing at all.
pub struct CatalogBuild {
    pub providers: Vec<ProviderModels>,
    pub degraded: Vec<String>,
    pub failed: Vec<String>,
}

/// Merges API-sourced model data with hand-owned quirks into the full
/// provider catalog, in the fixed provider order of [`PROVIDER_SOURCES`].
///
/// Per provider: API models (models.dev primary, OpenRouter gap-fill) are
/// curated by `exclude` globs and `rules`, hand-owned `models:` entries are
/// merged in field-wise (hand-owned wins) and placed first, API models are
/// sorted newest-first by release date (name-ascending tiebreak, undated
/// last), and `variants` clones are inserted immediately after their base
/// model. Providers without usable data are routed to `degraded` or `failed`
/// instead of erroring; a provider never emits zero models.
pub fn build_catalog(sources: &FetchedSources, quirks: &[ProviderQuirks]) -> CatalogBuild {
    let modelsdev: Option<Value> = sources
        .modelsdev
        .as_deref()
        .and_then(|payload| serde_json::from_str(payload).ok());
    let openrouter: Option<Value> = sources
        .openrouter
        .as_deref()
        .and_then(|payload| serde_json::from_str(payload).ok());
    let or_index = openrouter
        .as_ref()
        .map(openrouter_index)
        .unwrap_or_default();

    let mut providers = Vec::new();
    let mut degraded = Vec::new();
    let mut failed = Vec::new();

    for source in &PROVIDER_SOURCES {
        let provider_quirks = quirks_for(quirks, source.provider);
        let api_mapped = source.provider == "openrouter" || source.modelsdev.is_some();

        let mut api_models: Vec<SourcedModel> = if source.provider == "openrouter" {
            openrouter
                .as_ref()
                .map(openrouter_native_models)
                .unwrap_or_default()
        } else if let (Some(id), Some(api)) = (source.modelsdev, modelsdev.as_ref()) {
            let mut models = modelsdev_provider_models(api, id);
            if let Some(prefix) = source.openrouter {
                for model in &mut models {
                    if let Some(entry) =
                        or_index.get(format!("{prefix}/{}", model.data.name).as_str())
                    {
                        fill_from_openrouter(&mut model.data, entry);
                    }
                }
            }
            models
        } else {
            Vec::new()
        };

        if let Some(provider_quirks) = provider_quirks {
            api_models.retain(|model| {
                !provider_quirks
                    .exclude
                    .iter()
                    .any(|pattern| glob_matches(pattern, &model.data.name))
            });
            for model in &mut api_models {
                apply_rules(provider_quirks, &mut model.data);
            }
        }

        let hand_owned = provider_quirks
            .map(|provider_quirks| provider_quirks.models.clone())
            .unwrap_or_default();

        if api_models.is_empty() {
            if hand_owned.is_empty() {
                failed.push(source.provider.to_string());
                continue;
            }
            if api_mapped {
                degraded.push(source.provider.to_string());
            }
        }

        let mut merged_hand = Vec::with_capacity(hand_owned.len());
        for hand in hand_owned {
            match api_models
                .iter()
                .position(|model| model.data.name == hand.name)
            {
                Some(position) => {
                    let api = api_models.remove(position);
                    merged_hand.push(overlay_hand_owned(hand, api.data));
                }
                None => merged_hand.push(hand),
            }
        }

        api_models.sort_by(|a, b| match (&a.release_date, &b.release_date) {
            (Some(a_date), Some(b_date)) => b_date
                .cmp(a_date)
                .then_with(|| a.data.name.cmp(&b.data.name)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.data.name.cmp(&b.data.name),
        });

        let bases = merged_hand
            .into_iter()
            .chain(api_models.into_iter().map(|model| model.data));

        let variants = provider_quirks
            .map(|provider_quirks| provider_quirks.variants.as_slice())
            .unwrap_or_default();
        let mut models = Vec::new();
        for base in bases {
            let mut clones = Vec::new();
            for variant in variants {
                if !glob_matches(&variant.r#match, &base.name) {
                    continue;
                }
                let mut clone = base.clone();
                clone.name = format!("{}{}", base.name, variant.suffix);
                // Without an explicit real_name the clone would send its
                // suffixed name on the wire; default to the base's wire name.
                if clone.real_name.is_none() {
                    clone.real_name = Some(base.name.clone());
                }
                apply_variant(variant, &mut clone);
                clones.push(clone);
            }
            models.push(base);
            models.append(&mut clones);
        }

        providers.push(ProviderModels {
            provider: source.provider.to_string(),
            oauth: provider_quirks.and_then(|provider_quirks| provider_quirks.oauth.clone()),
            models,
        });
    }

    CatalogBuild {
        providers,
        degraded,
        failed,
    }
}

/// Like [`build_catalog`], but errors unless every provider was assembled
/// from live API data (or is quirks-owned by design).
#[allow(dead_code)]
pub fn build_catalog_strict(
    sources: &FetchedSources,
    quirks: &[ProviderQuirks],
) -> Result<Vec<ProviderModels>> {
    let build = build_catalog(sources, quirks);
    if !build.degraded.is_empty() || !build.failed.is_empty() {
        bail!(
            "model catalog build incomplete: degraded providers [{}], failed providers [{}]",
            build.degraded.join(", "),
            build.failed.join(", ")
        );
    }
    Ok(build.providers)
}

/// Fraction of `old` names that also appear in `new`; empty `old` yields 1.0.
/// Used to guard against upstream id-shape drift: a low overlap means the
/// upstream catalog renamed its models out from under us.
#[allow(dead_code)]
pub(crate) fn name_overlap(old: &[String], new: &[String]) -> f64 {
    if old.is_empty() {
        return 1.0;
    }
    let new_names: std::collections::HashSet<&str> = new.iter().map(String::as_str).collect();
    let kept = old
        .iter()
        .filter(|name| new_names.contains(name.as_str()))
        .count();
    kept as f64 / old.len() as f64
}

/// Overlays a hand-owned quirks entry onto the API-sourced model of the same
/// name. Every field the hand-owned entry carries (Some, true, or non-empty)
/// wins; absent fields keep the API value. `model_type` is authoritative on
/// the hand-owned entry.
fn overlay_hand_owned(hand: ModelData, api: ModelData) -> ModelData {
    ModelData {
        name: hand.name,
        model_type: hand.model_type,
        real_name: hand.real_name.or(api.real_name),
        max_input_tokens: hand.max_input_tokens.or(api.max_input_tokens),
        input_price: hand.input_price.or(api.input_price),
        output_price: hand.output_price.or(api.output_price),
        patch: hand.patch.or(api.patch),
        max_output_tokens: hand.max_output_tokens.or(api.max_output_tokens),
        require_max_tokens: hand.require_max_tokens || api.require_max_tokens,
        supports_vision: hand.supports_vision || api.supports_vision,
        supports_function_calling: hand.supports_function_calling || api.supports_function_calling,
        reasoning_levels: if hand.reasoning_levels.is_empty() {
            api.reasoning_levels
        } else {
            hand.reasoning_levels
        },
        default_reasoning_effort: hand
            .default_reasoning_effort
            .or(api.default_reasoning_effort),
        no_stream: hand.no_stream || api.no_stream,
        no_system_message: hand.no_system_message || api.no_system_message,
        system_prompt_prefix: hand.system_prompt_prefix.or(api.system_prompt_prefix),
        max_tokens_per_chunk: hand.max_tokens_per_chunk.or(api.max_tokens_per_chunk),
        default_chunk_size: hand.default_chunk_size.or(api.default_chunk_size),
        max_batch_size: hand.max_batch_size.or(api.max_batch_size),
    }
}

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

/// Overlays a variant's explicitly present fields onto a cloned model; same
/// field semantics as [`apply_rules`].
fn apply_variant(variant: &QuirkVariant, data: &mut ModelData) {
    if let Some(real_name) = &variant.real_name {
        data.real_name = Some(real_name.clone());
    }
    if let Some(max_input_tokens) = variant.max_input_tokens {
        data.max_input_tokens = Some(max_input_tokens);
    }
    if let Some(max_output_tokens) = variant.max_output_tokens {
        data.max_output_tokens = Some(max_output_tokens);
    }
    if let Some(input_price) = variant.input_price {
        data.input_price = Some(input_price);
    }
    if let Some(output_price) = variant.output_price {
        data.output_price = Some(output_price);
    }
    if let Some(patch) = &variant.patch {
        data.patch = Some(patch.clone());
    }
    if let Some(require_max_tokens) = variant.require_max_tokens {
        data.require_max_tokens = require_max_tokens;
    }
    if let Some(supports_vision) = variant.supports_vision {
        data.supports_vision = supports_vision;
    }
    if let Some(supports_function_calling) = variant.supports_function_calling {
        data.supports_function_calling = supports_function_calling;
    }
    if let Some(reasoning_levels) = &variant.reasoning_levels {
        data.reasoning_levels = reasoning_levels.clone();
    }
    if let Some(default_reasoning_effort) = &variant.default_reasoning_effort {
        data.default_reasoning_effort = Some(default_reasoning_effort.clone());
    }
    if let Some(no_stream) = variant.no_stream {
        data.no_stream = no_stream;
    }
    if let Some(no_system_message) = variant.no_system_message {
        data.no_system_message = no_system_message;
    }
    if let Some(system_prompt_prefix) = &variant.system_prompt_prefix {
        data.system_prompt_prefix = Some(system_prompt_prefix.clone());
    }
}

/// Finds the quirks for a client: an exact provider match wins, otherwise a
/// client whose name starts with the provider name (openai-compatible clients
/// are conventionally named after the provider they wrap).
pub fn quirks_for<'a>(
    quirks: &'a [ProviderQuirks],
    client_name: &str,
) -> Option<&'a ProviderQuirks> {
    quirks
        .iter()
        .find(|quirks| quirks.provider == client_name)
        .or_else(|| {
            quirks
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

    use serde_json::json;

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

    const MODELSDEV_FIXTURE: &str = include_str!("testdata/modelsdev_fixture.json");
    const OPENROUTER_FIXTURE: &str = include_str!("testdata/openrouter_fixture.json");

    fn make_sources(modelsdev: Option<&str>, openrouter: Option<&str>) -> FetchedSources {
        FetchedSources {
            modelsdev: modelsdev.map(str::to_string),
            openrouter: openrouter.map(str::to_string),
        }
    }

    fn fixture_sources() -> FetchedSources {
        make_sources(Some(MODELSDEV_FIXTURE), Some(OPENROUTER_FIXTURE))
    }

    fn parse_quirks(yaml: &str) -> Vec<ProviderQuirks> {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn find_provider<'a>(build: &'a CatalogBuild, provider: &str) -> &'a ProviderModels {
        build
            .providers
            .iter()
            .find(|candidate| candidate.provider == provider)
            .unwrap_or_else(|| panic!("provider '{provider}' missing"))
    }

    fn find_model<'a>(provider: &'a ProviderModels, name: &str) -> &'a ModelData {
        provider
            .models
            .iter()
            .find(|model| model.name == name)
            .unwrap_or_else(|| panic!("model '{name}' missing"))
    }

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
    fn provider_sources_match_known_providers() {
        let order: Vec<&str> = PROVIDER_SOURCES
            .iter()
            .map(|source| source.provider)
            .collect();
        assert_eq!(order, KNOWN_PROVIDERS);
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

    #[test]
    fn modelsdev_adapter_maps_fields() {
        let api: Value = serde_json::from_str(MODELSDEV_FIXTURE).unwrap();

        let openai = modelsdev_provider_models(&api, "openai");
        let gpt56 = openai
            .iter()
            .find(|model| model.data.name == "gpt-5.6")
            .unwrap();
        assert_eq!(gpt56.data.max_input_tokens, Some(1_050_000));
        assert_eq!(gpt56.data.max_output_tokens, Some(128_000));
        assert_eq!(gpt56.data.input_price, Some(4.0));
        assert_eq!(gpt56.data.output_price, Some(20.0));
        assert!(gpt56.data.supports_vision);
        assert!(gpt56.data.supports_function_calling);
        assert_eq!(
            gpt56.data.reasoning_levels,
            ["none", "low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(gpt56.release_date.as_deref(), Some("2026-07-09"));

        let alibaba = modelsdev_provider_models(&api, "alibaba");
        assert_eq!(alibaba.len(), 1);
        let qwen = &alibaba[0];
        assert_eq!(qwen.data.name, "qwen-max");
        assert_eq!(qwen.data.max_input_tokens, Some(32_768));
        assert_eq!(qwen.data.max_output_tokens, Some(8_192));
        assert_eq!(qwen.data.input_price, Some(1.6));
        assert_eq!(qwen.data.output_price, Some(6.4));
        assert!(!qwen.data.supports_vision);
        assert!(qwen.data.supports_function_calling);
        assert!(qwen.data.reasoning_levels.is_empty());
        assert_eq!(qwen.release_date.as_deref(), Some("2024-04-03"));

        assert!(modelsdev_provider_models(&api, "no-such-provider").is_empty());
    }

    #[test]
    fn openrouter_adapter_maps_fields() {
        let payload: Value = serde_json::from_str(OPENROUTER_FIXTURE).unwrap();
        let index = openrouter_index(&payload);

        let data = openrouter_model_data("gpt-4.1", index["openai/gpt-4.1"]);
        assert_eq!(data.max_input_tokens, Some(1_047_576));
        assert_eq!(data.max_output_tokens, Some(32_768));
        assert_eq!(data.input_price, Some(2.0));
        assert_eq!(data.output_price, Some(8.0));
        assert!(data.supports_vision);
        assert!(data.supports_function_calling);

        let rounded = openrouter_model_data(
            "m",
            &json!({"pricing": {"prompt": "0.00000123456789", "completion": "0.0000025"}}),
        );
        assert_eq!(rounded.input_price, Some(1.2346));
        assert_eq!(rounded.output_price, Some(2.5));
    }

    #[test]
    fn clamps_drop_out_of_range_fields() {
        let api = json!({
            "p": {"models": {"m": {
                "limit": {"context": 100_000_000, "output": 0},
                "cost": {"input": -1.0, "output": 20_000.0},
            }}}
        });
        let models = modelsdev_provider_models(&api, "p");
        assert_eq!(models.len(), 1);
        let data = &models[0].data;
        assert_eq!(data.name, "m");
        assert_eq!(data.max_input_tokens, None);
        assert_eq!(data.max_output_tokens, None);
        assert_eq!(data.input_price, None);
        assert_eq!(data.output_price, None);

        let data = openrouter_model_data(
            "m",
            &json!({
                "context_length": 60_000_000,
                "top_provider": {"max_completion_tokens": 20_000_000},
                "pricing": {"prompt": "-0.000001", "completion": "999"},
            }),
        );
        assert_eq!(data.max_input_tokens, None);
        assert_eq!(data.max_output_tokens, None);
        assert_eq!(data.input_price, None);
        assert_eq!(data.output_price, None);
    }

    #[test]
    fn adapters_never_set_quirks_owned_fields() {
        let api: Value = serde_json::from_str(MODELSDEV_FIXTURE).unwrap();
        let payload: Value = serde_json::from_str(OPENROUTER_FIXTURE).unwrap();
        let mut all: Vec<SourcedModel> = ["openai", "anthropic", "xai", "alibaba"]
            .iter()
            .flat_map(|id| modelsdev_provider_models(&api, id))
            .collect();
        all.extend(openrouter_native_models(&payload));
        assert!(!all.is_empty());
        for model in &all {
            let data = &model.data;
            assert_eq!(data.model_type, "chat", "{}", data.name);
            assert!(data.patch.is_none(), "{}", data.name);
            assert!(data.real_name.is_none(), "{}", data.name);
            assert!(data.system_prompt_prefix.is_none(), "{}", data.name);
            assert!(data.max_tokens_per_chunk.is_none(), "{}", data.name);
            assert!(data.default_chunk_size.is_none(), "{}", data.name);
            assert!(data.max_batch_size.is_none(), "{}", data.name);
        }
    }

    #[test]
    fn oauth_comes_only_from_quirks() {
        let without_quirks = build_catalog(&fixture_sources(), &[]);
        assert!(
            without_quirks
                .providers
                .iter()
                .all(|provider| provider.oauth.is_none())
        );

        let quirks = parse_quirks(
            r#"
- provider: xai
  oauth:
    client_id: id
    token_url: https://example.com/token
"#,
        );
        let build = build_catalog(&fixture_sources(), &quirks);
        assert!(find_provider(&build, "xai").oauth.is_some());
        assert!(find_provider(&build, "openai").oauth.is_none());
    }

    #[test]
    fn merge_precedence_hand_owned_beats_rule_beats_modelsdev_beats_openrouter() {
        let modelsdev = json!({
            "xai": {"models": {
                "m-1": {"limit": {"context": 1000}, "cost": {"input": 1.0}},
                "m-2": {"limit": {"context": 1000}},
            }}
        })
        .to_string();
        let openrouter = json!({"data": [
            {
                "id": "x-ai/m-1",
                "context_length": 2000,
                "pricing": {"prompt": "0.000003", "completion": "0.000004"},
                "top_provider": {"max_completion_tokens": 512},
            },
            {"id": "x-ai/m-2", "context_length": 2000},
        ]})
        .to_string();
        let quirks = parse_quirks(
            r#"
- provider: xai
  rules:
    - match: "m-1"
      input_price: 5
      max_input_tokens: 3000
  models:
    - name: m-1
      input_price: 7
"#,
        );
        let build = build_catalog(&make_sources(Some(&modelsdev), Some(&openrouter)), &quirks);
        let xai = find_provider(&build, "xai");

        let merged = find_model(xai, "m-1");
        // hand-owned beats the rule, which beat models.dev, which beat openrouter
        assert_eq!(merged.input_price, Some(7.0));
        // rule beats models.dev
        assert_eq!(merged.max_input_tokens, Some(3000));
        // openrouter fills fields models.dev left unset
        assert_eq!(merged.output_price, Some(4.0));
        assert_eq!(merged.max_output_tokens, Some(512));

        // models.dev beats openrouter on the same field
        assert_eq!(find_model(xai, "m-2").max_input_tokens, Some(1000));
    }

    #[test]
    fn excludes_drop_api_models_but_never_hand_owned() {
        let quirks = parse_quirks(
            r#"
- provider: openai
  exclude:
    - "gpt-4*"
  models:
    - name: gpt-4.1
      input_price: 9
"#,
        );
        let build = build_catalog(&fixture_sources(), &quirks);
        let openai = find_provider(&build, "openai");
        assert!(openai.models.iter().all(|model| model.name != "gpt-4o"));
        find_model(openai, "gpt-5.6");
        let hand = find_model(openai, "gpt-4.1");
        assert_eq!(hand.input_price, Some(9.0));
        // the API copy was excluded before the merge, so nothing leaked in
        assert_eq!(hand.max_input_tokens, None);
    }

    #[test]
    fn variant_expansion() {
        let quirks = parse_quirks(
            r#"
- provider: xai
  variants:
    - match: "grok-4.6"
      suffix: ":thinking"
      max_output_tokens: 24000
      patch:
        body:
          thinking: true
    - match: "grok-4.5"
      suffix: ":fast"
      real_name: grok-4.5-fast
"#,
        );
        let build = build_catalog(&fixture_sources(), &quirks);
        let xai = find_provider(&build, "xai");
        let names: Vec<&str> = xai.models.iter().map(|model| model.name.as_str()).collect();
        assert_eq!(
            names,
            ["grok-4.6", "grok-4.6:thinking", "grok-4.5", "grok-4.5:fast"]
        );

        let thinking = find_model(xai, "grok-4.6:thinking");
        assert_eq!(thinking.real_name.as_deref(), Some("grok-4.6"));
        assert_eq!(thinking.max_output_tokens, Some(24_000));
        assert_eq!(thinking.patch, Some(json!({"body": {"thinking": true}})));

        assert_eq!(
            find_model(xai, "grok-4.5:fast").real_name.as_deref(),
            Some("grok-4.5-fast")
        );
        assert_eq!(find_model(xai, "grok-4.6").real_name, None);
    }

    #[test]
    fn ordering_hand_owned_first_then_release_date_desc_name_asc() {
        let shuffled_payloads = [
            r#"{"openai":{"models":{
                "b":{"release_date":"2026-05-01"},
                "a":{"release_date":"2026-05-01"},
                "c":{"release_date":"2026-06-01"},
                "z":{},
                "y":{}
            }}}"#,
            r#"{"openai":{"models":{
                "y":{},
                "c":{"release_date":"2026-06-01"},
                "z":{},
                "a":{"release_date":"2026-05-01"},
                "b":{"release_date":"2026-05-01"}
            }}}"#,
        ];
        let quirks = parse_quirks(
            r#"
- provider: openai
  models:
    - name: n2
    - name: n1
"#,
        );
        for payload in shuffled_payloads {
            let build = build_catalog(&make_sources(Some(payload), None), &quirks);
            let names: Vec<&str> = find_provider(&build, "openai")
                .models
                .iter()
                .map(|model| model.name.as_str())
                .collect();
            assert_eq!(names, ["n2", "n1", "c", "a", "b", "y", "z"]);
        }
    }

    #[test]
    fn catalog_round_trips_through_yaml() {
        let build = build_catalog(&fixture_sources(), &ALL_QUIRKS);
        assert!(!build.providers.is_empty());
        let yaml = serde_yaml::to_string(&build.providers).unwrap();
        let parsed: Vec<ProviderModels> = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(serde_yaml::to_string(&parsed).unwrap(), yaml);
    }

    #[test]
    fn openrouter_native_models_use_full_ids() {
        let build = build_catalog(&fixture_sources(), &[]);
        let openrouter = find_provider(&build, "openrouter");
        assert_eq!(openrouter.models.len(), 8);

        let auto = find_model(openrouter, "openrouter/auto");
        assert_eq!(auto.max_input_tokens, Some(2_000_000));
        // null max_completion_tokens and the "-1" sentinel prices are dropped
        assert_eq!(auto.max_output_tokens, None);
        assert_eq!(auto.input_price, None);
        assert_eq!(auto.output_price, None);
        assert!(auto.supports_vision);
        assert!(auto.supports_function_calling);

        let free = find_model(openrouter, "openrouter/free");
        assert_eq!(free.max_input_tokens, Some(200_000));
        assert_eq!(free.input_price, Some(0.0));
        assert_eq!(free.output_price, Some(0.0));

        // no release dates upstream, so full ids sort ascending
        let names: Vec<&str> = openrouter
            .models
            .iter()
            .map(|model| model.name.as_str())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn failure_routing_degraded_and_failed() {
        let quirks = parse_quirks(
            r#"
- provider: gemini
  models: [{name: g-1}]
- provider: ai21
  models: [{name: jamba}]
"#,
        );
        let build = build_catalog(&make_sources(None, None), &quirks);
        assert_eq!(build.degraded, ["gemini"]);
        assert!(build.failed.contains(&"openai".to_string()));
        assert!(build.failed.contains(&"openrouter".to_string()));
        assert!(build.failed.contains(&"jina".to_string()));
        assert!(!build.failed.contains(&"gemini".to_string()));
        assert!(!build.failed.contains(&"ai21".to_string()));
        assert!(!build.degraded.contains(&"ai21".to_string()));

        let providers: Vec<&str> = build
            .providers
            .iter()
            .map(|provider| provider.provider.as_str())
            .collect();
        assert_eq!(providers, ["gemini", "ai21"]);
        assert!(
            build
                .providers
                .iter()
                .all(|provider| !provider.models.is_empty())
        );
    }

    #[test]
    fn strict_errors_on_degraded_or_failed() {
        let quirks = parse_quirks(
            r#"
- provider: gemini
  models: [{name: g-1}]
"#,
        );
        let err = build_catalog_strict(&make_sources(None, None), &quirks)
            .unwrap_err()
            .to_string();
        assert!(err.contains("gemini"), "{err}");
        assert!(err.contains("openai"), "{err}");
    }

    #[test]
    fn strict_succeeds_when_every_provider_resolves() {
        let mut api = serde_json::Map::new();
        for source in &PROVIDER_SOURCES {
            if let Some(id) = source.modelsdev {
                api.insert(id.to_string(), json!({"models": {"m": {}}}));
            }
        }
        let modelsdev = Value::Object(api).to_string();
        let openrouter = json!({"data": [{"id": "openrouter/auto"}]}).to_string();
        let quirks = parse_quirks(
            r#"
- provider: ai21
  models: [{name: a}]
- provider: ernie
  models: [{name: e}]
- provider: hunyuan
  models: [{name: h}]
- provider: jina
  models: [{name: j}]
- provider: voyageai
  models: [{name: v}]
"#,
        );
        let providers =
            build_catalog_strict(&make_sources(Some(&modelsdev), Some(&openrouter)), &quirks)
                .unwrap();
        let order: Vec<&str> = providers
            .iter()
            .map(|provider| provider.provider.as_str())
            .collect();
        assert_eq!(order, KNOWN_PROVIDERS);
    }

    #[test]
    fn name_overlap_fraction_of_old_names_still_present() {
        let old: Vec<String> = ["a", "b", "c", "d"].map(str::to_string).into();
        let new: Vec<String> = ["a", "c", "x"].map(str::to_string).into();
        assert_eq!(name_overlap(&old, &new), 0.5);
        assert_eq!(name_overlap(&[], &new), 1.0);
        assert_eq!(name_overlap(&old, &[]), 0.0);
    }

    /// Guards against upstream id-shape drift by comparing live API model
    /// names against the embedded models.yaml. Run manually:
    /// `cargo test idshape_guard_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn idshape_guard_live() {
        let sources = fetch_sources().await;
        let modelsdev: Value = serde_json::from_str(
            sources
                .modelsdev
                .as_deref()
                .expect("models.dev fetch failed"),
        )
        .unwrap();
        let openrouter: Value = serde_json::from_str(
            sources
                .openrouter
                .as_deref()
                .expect("openrouter fetch failed"),
        )
        .unwrap();
        let embedded: Vec<ProviderModels> =
            serde_yaml::from_str(crate::client::MODELS_YAML).unwrap();

        println!(
            "{:<12} {:>7} {:>5} {:>5}",
            "provider", "overlap", "old", "new"
        );
        for source in &PROVIDER_SOURCES {
            let new: Vec<String> = if source.provider == "openrouter" {
                openrouter_native_models(&openrouter)
                    .into_iter()
                    .map(|model| model.data.name)
                    .collect()
            } else if let Some(id) = source.modelsdev {
                modelsdev_provider_models(&modelsdev, id)
                    .into_iter()
                    .map(|model| model.data.name)
                    .collect()
            } else {
                continue;
            };
            let old: Vec<String> = embedded
                .iter()
                .find(|provider| provider.provider == source.provider)
                .map(|provider| {
                    provider
                        .models
                        .iter()
                        .filter(|model| {
                            model.model_type == "chat" && !model.name.ends_with(":thinking")
                        })
                        .map(|model| model.name.clone())
                        .collect()
                })
                .unwrap_or_default();
            println!(
                "{:<12} {:>7.2} {:>5} {:>5}",
                source.provider,
                name_overlap(&old, &new),
                old.len(),
                new.len()
            );
        }
    }
}
