//! Immutable, server-wide application configuration.
//!
//! `AppConfig` contains the settings loaded from `config.yaml` that are
//! global to the Loki process: LLM provider configs, UI preferences, tool
//! and MCP settings, RAG defaults, etc.
//!
//! This is Phase 1, Step 0 of the REST API refactor: the struct is
//! introduced alongside the existing [`Config`](super::Config) and is not
//! yet wired into the runtime. See `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
//! for the full migration plan.
//!
//! # Relationship to `Config`
//!
//! `AppConfig` mirrors the **serialized** fields of [`Config`] — that is,
//! every field that is NOT marked `#[serde(skip)]`. The deserialization
//! shape is identical so an existing `config.yaml` can be loaded into
//! either type without modification.
//!
//! Runtime-only state (current role, session, agent, supervisor, etc.)
//! lives on [`RequestContext`](super::request_context::RequestContext).

use crate::client::{ClientConfig, list_models};
use crate::render::{MarkdownRender, RenderOptions};
use crate::utils::{IS_STDOUT_TERMINAL, NO_COLOR, decode_bin, get_env_name};

use super::paths;
use anyhow::{Context, Result, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use syntect::highlighting::ThemeSet;
use terminal_colorsaurus::{ColorScheme, QueryOptions, color_scheme};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    #[serde(default)]
    #[allow(dead_code)]
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,

    pub dry_run: bool,
    pub stream: bool,
    pub save: bool,
    pub keybindings: String,
    pub editor: Option<String>,
    pub wrap: Option<String>,
    pub wrap_code: bool,
    pub(crate) vault_password_file: Option<PathBuf>,

    pub function_calling_support: bool,
    pub mapping_tools: IndexMap<String, String>,
    pub enabled_tools: Option<String>,
    pub visible_tools: Option<Vec<String>>,

    pub mcp_server_support: bool,
    pub mapping_mcp_servers: IndexMap<String, String>,
    pub enabled_mcp_servers: Option<String>,

    pub repl_prelude: Option<String>,
    pub cmd_prelude: Option<String>,
    pub agent_session: Option<String>,

    pub save_session: Option<bool>,
    pub compression_threshold: usize,
    pub summarization_prompt: Option<String>,
    pub summary_context_prompt: Option<String>,

    pub rag_embedding_model: Option<String>,
    pub rag_reranker_model: Option<String>,
    pub rag_top_k: usize,
    pub rag_chunk_size: Option<usize>,
    pub rag_chunk_overlap: Option<usize>,
    pub rag_template: Option<String>,

    #[serde(default)]
    pub document_loaders: HashMap<String, String>,

    pub highlight: bool,
    pub theme: Option<String>,
    pub left_prompt: Option<String>,
    pub right_prompt: Option<String>,

    pub user_agent: Option<String>,
    pub save_shell_history: bool,
    pub sync_models_url: Option<String>,

    pub clients: Vec<ClientConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model_id: Default::default(),
            temperature: None,
            top_p: None,

            dry_run: false,
            stream: true,
            save: false,
            keybindings: "emacs".into(),
            editor: None,
            wrap: None,
            wrap_code: false,
            vault_password_file: None,

            function_calling_support: true,
            mapping_tools: Default::default(),
            enabled_tools: None,
            visible_tools: None,

            mcp_server_support: true,
            mapping_mcp_servers: Default::default(),
            enabled_mcp_servers: None,

            repl_prelude: None,
            cmd_prelude: None,
            agent_session: None,

            save_session: None,
            compression_threshold: 4000,
            summarization_prompt: None,
            summary_context_prompt: None,

            rag_embedding_model: None,
            rag_reranker_model: None,
            rag_top_k: 5,
            rag_chunk_size: None,
            rag_chunk_overlap: None,
            rag_template: None,

            document_loaders: Default::default(),

            highlight: true,
            theme: None,
            left_prompt: None,
            right_prompt: None,

            user_agent: None,
            save_shell_history: true,
            sync_models_url: None,

            clients: vec![],
        }
    }
}

impl AppConfig {
    #[allow(dead_code)]
    pub fn from_config(config: super::Config) -> Result<Self> {
        let mut app_config = config.to_app_config();
        app_config.load_envs();
        if let Some(wrap) = app_config.wrap.clone() {
            app_config.set_wrap(&wrap)?;
        }
        app_config.setup_document_loaders();
        app_config.setup_user_agent();
        app_config.resolve_model()?;
        Ok(app_config)
    }

    #[allow(dead_code)]
    pub fn resolve_model(&mut self) -> Result<()> {
        if self.model_id.is_empty() {
            let models = list_models(self, crate::client::ModelType::Chat);
            if models.is_empty() {
                anyhow::bail!("No available model");
            }
            self.model_id = models[0].id();
        }
        Ok(())
    }

    pub fn vault_password_file(&self) -> PathBuf {
        match &self.vault_password_file {
            Some(path) => match path.exists() {
                true => path.clone(),
                false => gman::config::Config::local_provider_password_file(),
            },
            None => gman::config::Config::local_provider_password_file(),
        }
    }

    pub fn editor(&self) -> Result<String> {
        super::EDITOR.get_or_init(move || {
            let editor = self.editor.clone()
                .or_else(|| env::var("VISUAL").ok().or_else(|| env::var("EDITOR").ok()))
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        "notepad".to_string()
                    } else {
                        "nano".to_string()
                    }
                });
            which::which(&editor).ok().map(|_| editor)
        })
            .clone()
            .ok_or_else(|| anyhow!("Editor not found. Please add the `editor` configuration or set the $EDITOR or $VISUAL environment variable."))
    }

    pub fn sync_models_url(&self) -> String {
        self.sync_models_url
            .clone()
            .unwrap_or_else(|| super::SYNC_MODELS_URL.into())
    }

    pub fn light_theme(&self) -> bool {
        matches!(self.theme.as_deref(), Some("light"))
    }

    pub fn render_options(&self) -> Result<RenderOptions> {
        let theme = if self.highlight {
            let theme_mode = if self.light_theme() { "light" } else { "dark" };
            let theme_filename = format!("{theme_mode}.tmTheme");
            let theme_path = paths::local_path(&theme_filename);
            if theme_path.exists() {
                let theme = ThemeSet::get_theme(&theme_path)
                    .with_context(|| format!("Invalid theme at '{}'", theme_path.display()))?;
                Some(theme)
            } else {
                let theme = if self.light_theme() {
                    decode_bin(super::LIGHT_THEME).context("Invalid builtin light theme")?
                } else {
                    decode_bin(super::DARK_THEME).context("Invalid builtin dark theme")?
                };
                Some(theme)
            }
        } else {
            None
        };
        let wrap = if *IS_STDOUT_TERMINAL {
            self.wrap.clone()
        } else {
            None
        };
        let truecolor = matches!(
            env::var("COLORTERM").as_ref().map(|v| v.as_str()),
            Ok("truecolor")
        );
        Ok(RenderOptions::new(theme, wrap, self.wrap_code, truecolor))
    }

    pub fn print_markdown(&self, text: &str) -> Result<()> {
        if *IS_STDOUT_TERMINAL {
            let render_options = self.render_options()?;
            let mut markdown_render = MarkdownRender::init(render_options)?;
            println!("{}", markdown_render.render(text));
        } else {
            println!("{text}");
        }
        Ok(())
    }
}

impl AppConfig {
    pub fn set_wrap(&mut self, value: &str) -> Result<()> {
        if value == "no" {
            self.wrap = None;
        } else if value == "auto" {
            self.wrap = Some(value.into());
        } else {
            value
                .parse::<u16>()
                .map_err(|_| anyhow!("Invalid wrap value"))?;
            self.wrap = Some(value.into())
        }
        Ok(())
    }

    pub fn setup_document_loaders(&mut self) {
        [("pdf", "pdftotext $1 -"), ("docx", "pandoc --to plain $1")]
            .into_iter()
            .for_each(|(k, v)| {
                let (k, v) = (k.to_string(), v.to_string());
                self.document_loaders.entry(k).or_insert(v);
            });
    }

    pub fn setup_user_agent(&mut self) {
        if let Some("auto") = self.user_agent.as_deref() {
            self.user_agent = Some(format!(
                "{}/{}",
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION")
            ));
        }
    }

    pub fn load_envs(&mut self) {
        if let Ok(v) = env::var(get_env_name("model")) {
            self.model_id = v;
        }
        if let Some(v) = super::read_env_value::<f64>(&get_env_name("temperature")) {
            self.temperature = v;
        }
        if let Some(v) = super::read_env_value::<f64>(&get_env_name("top_p")) {
            self.top_p = v;
        }

        if let Some(Some(v)) = super::read_env_bool(&get_env_name("dry_run")) {
            self.dry_run = v;
        }
        if let Some(Some(v)) = super::read_env_bool(&get_env_name("stream")) {
            self.stream = v;
        }
        if let Some(Some(v)) = super::read_env_bool(&get_env_name("save")) {
            self.save = v;
        }
        if let Ok(v) = env::var(get_env_name("keybindings"))
            && v == "vi"
        {
            self.keybindings = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("editor")) {
            self.editor = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("wrap")) {
            self.wrap = v;
        }
        if let Some(Some(v)) = super::read_env_bool(&get_env_name("wrap_code")) {
            self.wrap_code = v;
        }

        if let Some(Some(v)) = super::read_env_bool(&get_env_name("function_calling_support")) {
            self.function_calling_support = v;
        }
        if let Ok(v) = env::var(get_env_name("mapping_tools"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.mapping_tools = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("enabled_tools")) {
            self.enabled_tools = v;
        }

        if let Some(Some(v)) = super::read_env_bool(&get_env_name("mcp_server_support")) {
            self.mcp_server_support = v;
        }
        if let Ok(v) = env::var(get_env_name("mapping_mcp_servers"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.mapping_mcp_servers = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("enabled_mcp_servers")) {
            self.enabled_mcp_servers = v;
        }

        if let Some(v) = super::read_env_value::<String>(&get_env_name("repl_prelude")) {
            self.repl_prelude = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("cmd_prelude")) {
            self.cmd_prelude = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("agent_session")) {
            self.agent_session = v;
        }

        if let Some(v) = super::read_env_bool(&get_env_name("save_session")) {
            self.save_session = v;
        }
        if let Some(Some(v)) =
            super::read_env_value::<usize>(&get_env_name("compression_threshold"))
        {
            self.compression_threshold = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("summarization_prompt")) {
            self.summarization_prompt = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("summary_context_prompt")) {
            self.summary_context_prompt = v;
        }

        if let Some(v) = super::read_env_value::<String>(&get_env_name("rag_embedding_model")) {
            self.rag_embedding_model = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("rag_reranker_model")) {
            self.rag_reranker_model = v;
        }
        if let Some(Some(v)) = super::read_env_value::<usize>(&get_env_name("rag_top_k")) {
            self.rag_top_k = v;
        }
        if let Some(v) = super::read_env_value::<usize>(&get_env_name("rag_chunk_size")) {
            self.rag_chunk_size = v;
        }
        if let Some(v) = super::read_env_value::<usize>(&get_env_name("rag_chunk_overlap")) {
            self.rag_chunk_overlap = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("rag_template")) {
            self.rag_template = v;
        }

        if let Ok(v) = env::var(get_env_name("document_loaders"))
            && let Ok(v) = serde_json::from_str(&v)
        {
            self.document_loaders = v;
        }

        if let Some(Some(v)) = super::read_env_bool(&get_env_name("highlight")) {
            self.highlight = v;
        }
        if *NO_COLOR {
            self.highlight = false;
        }
        if self.highlight && self.theme.is_none() {
            if let Some(v) = super::read_env_value::<String>(&get_env_name("theme")) {
                self.theme = v;
            } else if *IS_STDOUT_TERMINAL
                && let Ok(color_scheme) = color_scheme(QueryOptions::default())
            {
                let theme = match color_scheme {
                    ColorScheme::Dark => "dark",
                    ColorScheme::Light => "light",
                };
                self.theme = Some(theme.into());
            }
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("left_prompt")) {
            self.left_prompt = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("right_prompt")) {
            self.right_prompt = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("user_agent")) {
            self.user_agent = v;
        }
        if let Some(Some(v)) = super::read_env_bool(&get_env_name("save_shell_history")) {
            self.save_shell_history = v;
        }
        if let Some(v) = super::read_env_value::<String>(&get_env_name("sync_models_url")) {
            self.sync_models_url = v;
        }
    }
}

impl AppConfig {
    #[allow(dead_code)]
    pub fn set_temperature_default(&mut self, value: Option<f64>) {
        self.temperature = value;
    }

    #[allow(dead_code)]
    pub fn set_top_p_default(&mut self, value: Option<f64>) {
        self.top_p = value;
    }

    #[allow(dead_code)]
    pub fn set_enabled_tools_default(&mut self, value: Option<String>) {
        self.enabled_tools = value;
    }

    #[allow(dead_code)]
    pub fn set_enabled_mcp_servers_default(&mut self, value: Option<String>) {
        self.enabled_mcp_servers = value;
    }

    #[allow(dead_code)]
    pub fn set_save_session_default(&mut self, value: Option<bool>) {
        self.save_session = value;
    }

    #[allow(dead_code)]
    pub fn set_compression_threshold_default(&mut self, value: Option<usize>) {
        self.compression_threshold = value.unwrap_or_default();
    }

    #[allow(dead_code)]
    pub fn set_rag_reranker_model_default(&mut self, value: Option<String>) {
        self.rag_reranker_model = value;
    }

    #[allow(dead_code)]
    pub fn set_rag_top_k_default(&mut self, value: usize) {
        self.rag_top_k = value;
    }

    #[allow(dead_code)]
    pub fn set_model_id_default(&mut self, model_id: String) {
        self.model_id = model_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cached_editor() -> Option<String> {
        super::super::EDITOR.get().cloned().flatten()
    }

    #[test]
    fn to_app_config_copies_serialized_fields() {
        let cfg = Config {
            model_id: "test-model".to_string(),
            temperature: Some(0.7),
            top_p: Some(0.9),
            dry_run: true,
            stream: false,
            save: true,
            highlight: false,
            compression_threshold: 2000,
            rag_top_k: 10,
            ..Config::default()
        };

        let app = cfg.to_app_config();

        assert_eq!(app.model_id, "test-model");
        assert_eq!(app.temperature, Some(0.7));
        assert_eq!(app.top_p, Some(0.9));
        assert!(app.dry_run);
        assert!(!app.stream);
        assert!(app.save);
        assert!(!app.highlight);
        assert_eq!(app.compression_threshold, 2000);
        assert_eq!(app.rag_top_k, 10);
    }

    #[test]
    fn to_app_config_copies_clients() {
        let cfg = Config::default();
        let app = cfg.to_app_config();

        assert!(app.clients.is_empty());
    }

    #[test]
    fn to_app_config_copies_mapping_fields() {
        let mut cfg = Config::default();
        cfg.mapping_tools
            .insert("alias".to_string(), "real_tool".to_string());
        cfg.mapping_mcp_servers
            .insert("gh".to_string(), "github-mcp".to_string());

        let app = cfg.to_app_config();

        assert_eq!(
            app.mapping_tools.get("alias"),
            Some(&"real_tool".to_string())
        );
        assert_eq!(
            app.mapping_mcp_servers.get("gh"),
            Some(&"github-mcp".to_string())
        );
    }

    #[test]
    fn editor_returns_configured_value() {
        let configured = cached_editor()
            .unwrap_or_else(|| std::env::current_exe().unwrap().display().to_string());
        let app = AppConfig {
            editor: Some(configured.clone()),
            ..AppConfig::default()
        };

        assert_eq!(app.editor().unwrap(), configured);
    }

    #[test]
    fn editor_falls_back_to_env() {
        if let Some(expected) = cached_editor() {
            let app = AppConfig::default();
            assert_eq!(app.editor().unwrap(), expected);
            return;
        }

        let expected = std::env::current_exe().unwrap().display().to_string();
        unsafe {
            std::env::set_var("VISUAL", &expected);
        }

        let app = AppConfig::default();
        let result = app.editor();

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn light_theme_default_is_false() {
        let app = AppConfig::default();
        assert!(!app.light_theme());
    }

    #[test]
    fn sync_models_url_has_default() {
        let app = AppConfig::default();
        let url = app.sync_models_url();
        assert!(!url.is_empty());
    }

    #[test]
    fn from_config_copies_serde_fields() {
        let cfg = Config {
            model_id: "provider:model-x".to_string(),
            temperature: Some(0.42),
            compression_threshold: 1234,
            ..Config::default()
        };

        let app = AppConfig::from_config(cfg).unwrap();

        assert_eq!(app.model_id, "provider:model-x");
        assert_eq!(app.temperature, Some(0.42));
        assert_eq!(app.compression_threshold, 1234);
    }

    #[test]
    fn from_config_installs_default_document_loaders() {
        let cfg = Config {
            model_id: "provider:test".to_string(),
            ..Config::default()
        };
        let app = AppConfig::from_config(cfg).unwrap();

        assert_eq!(
            app.document_loaders.get("pdf"),
            Some(&"pdftotext $1 -".to_string())
        );
        assert_eq!(
            app.document_loaders.get("docx"),
            Some(&"pandoc --to plain $1".to_string())
        );
    }

    #[test]
    fn from_config_resolves_auto_user_agent() {
        let cfg = Config {
            model_id: "provider:test".to_string(),
            user_agent: Some("auto".to_string()),
            ..Config::default()
        };

        let app = AppConfig::from_config(cfg).unwrap();

        let ua = app.user_agent.as_deref().unwrap();
        assert!(ua != "auto", "user_agent should have been resolved");
        assert!(ua.contains('/'), "user_agent should be '<name>/<version>'");
    }

    #[test]
    fn from_config_preserves_explicit_user_agent() {
        let cfg = Config {
            model_id: "provider:test".to_string(),
            user_agent: Some("custom/1.0".to_string()),
            ..Config::default()
        };

        let app = AppConfig::from_config(cfg).unwrap();

        assert_eq!(app.user_agent.as_deref(), Some("custom/1.0"));
    }

    #[test]
    fn from_config_validates_wrap_value() {
        let cfg = Config {
            model_id: "provider:test".to_string(),
            wrap: Some("invalid".to_string()),
            ..Config::default()
        };

        let result = AppConfig::from_config(cfg);
        assert!(result.is_err());
    }

    #[test]
    fn from_config_accepts_wrap_auto() {
        let cfg = Config {
            model_id: "provider:test".to_string(),
            wrap: Some("auto".to_string()),
            ..Config::default()
        };

        let app = AppConfig::from_config(cfg).unwrap();
        assert_eq!(app.wrap.as_deref(), Some("auto"));
    }

    #[test]
    fn resolve_model_errors_when_no_models_available() {
        let mut app = AppConfig {
            model_id: String::new(),
            clients: vec![],
            ..AppConfig::default()
        };

        let result = app.resolve_model();
        assert!(result.is_err());
    }

    #[test]
    fn resolve_model_keeps_explicit_model_id() {
        let mut app = AppConfig {
            model_id: "provider:explicit".to_string(),
            ..AppConfig::default()
        };

        app.resolve_model().unwrap();
        assert_eq!(app.model_id, "provider:explicit");
    }
}
