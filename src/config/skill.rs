use super::*;

use anyhow::Result;
use fancy_regex::Regex;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Embed)]
#[folder = "assets/skills/"]
struct SkillsAsset;

static RE_METADATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)-{3,}\s*(.*?)\s*-{3,}\s*(.*)").unwrap());

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Skill {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_tools: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_mcp_servers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_unload: Option<bool>,
}

#[allow(dead_code)]
impl Skill {
    pub fn new(name: &str, content: &str) -> Self {
        let mut metadata = "";
        let mut body = content.trim();
        if let Ok(Some(caps)) = RE_METADATA.captures(content)
            && let (Some(metadata_value), Some(body_value)) = (caps.get(1), caps.get(2))
        {
            metadata = metadata_value.as_str().trim();
            body = body_value.as_str().trim();
        }
        let mut body = body.to_string();
        interpolate_variables(&mut body);
        let mut skill = Self {
            name: name.to_string(),
            body,
            ..Default::default()
        };
        if !metadata.is_empty()
            && let Ok(value) = serde_yaml::from_str::<Value>(metadata)
            && let Some(value) = value.as_object()
        {
            for (key, value) in value {
                match key.as_str() {
                    "description" => {
                        if let Some(v) = value.as_str() {
                            skill.description = v.to_string();
                        }
                    }
                    "enabled_tools" => {
                        skill.enabled_tools = value.as_str().map(|v| v.to_string());
                    }
                    "enabled_mcp_servers" => {
                        skill.enabled_mcp_servers = value.as_str().map(|v| v.to_string());
                    }
                    "auto_unload" => {
                        skill.auto_unload = value.as_bool();
                    }
                    _ => (),
                }
            }
        }
        skill
    }

    pub fn builtin(name: &str) -> Result<Self> {
        let content = SkillsAsset::get(&format!("{name}/SKILL.md"))
            .ok_or_else(|| anyhow!("Unknown skill `{name}`"))?;
        let content = unsafe { std::str::from_utf8_unchecked(&content.data) };
        Ok(Skill::new(name, content))
    }

    pub fn load(name: &str) -> Result<Self> {
        let path = paths::skill_file(name);
        let content = read_to_string(&path).with_context(|| {
            format!("Failed to read skill '{name}' at {}", path.display())
        })?;
        Ok(Skill::new(name, &content))
    }

    pub fn list_builtin_skill_names() -> Vec<String> {
        SkillsAsset::iter()
            .filter_map(|v| v.strip_suffix("/SKILL.md").map(|v| v.to_string()))
            .collect()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn enabled_tools(&self) -> Option<&str> {
        self.enabled_tools.as_deref()
    }

    pub fn enabled_mcp_servers(&self) -> Option<&str> {
        self.enabled_mcp_servers.as_deref()
    }

    pub fn auto_unload(&self) -> bool {
        self.auto_unload.unwrap_or(false)
    }

    pub fn is_compatible(&self, function_calling_enabled: bool, mcp_enabled: bool) -> bool {
        if self.declares_tools() && !function_calling_enabled {
            return false;
        }
        if self.declares_mcp_servers() && !mcp_enabled {
            return false;
        }
        true
    }

    fn declares_tools(&self) -> bool {
        self.enabled_tools
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }

    fn declares_mcp_servers(&self) -> bool {
        self.enabled_mcp_servers
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_new_parses_body() {
        let skill = Skill::new("test", "You are a git expert");

        assert_eq!(skill.name(), "test");
        assert_eq!(skill.body(), "You are a git expert");
        assert_eq!(skill.description(), "");
    }

    #[test]
    fn skill_new_parses_full_metadata() {
        let content = "---\n\
            description: Atomic commits, rebase surgery\n\
            enabled_tools: shell,fs\n\
            enabled_mcp_servers: github\n\
            auto_unload: true\n\
            ---\n\
            You are a git expert";

        let skill = Skill::new("git-master", content);

        assert_eq!(skill.name(), "git-master");
        assert_eq!(skill.description(), "Atomic commits, rebase surgery");
        assert_eq!(skill.enabled_tools(), Some("shell,fs"));
        assert_eq!(skill.enabled_mcp_servers(), Some("github"));
        assert!(skill.auto_unload());
        assert_eq!(skill.body(), "You are a git expert");
    }

    #[test]
    fn skill_new_no_metadata_has_defaults() {
        let skill = Skill::new("test", "Just a body");

        assert_eq!(skill.description(), "");
        assert_eq!(skill.enabled_tools(), None);
        assert_eq!(skill.enabled_mcp_servers(), None);
        assert!(!skill.auto_unload());
    }

    #[test]
    fn skill_new_metadata_only() {
        let content = "---\ndescription: Just metadata\n---";

        let skill = Skill::new("test", content);

        assert_eq!(skill.description(), "Just metadata");
        assert_eq!(skill.body(), "");
    }

    #[test]
    fn skill_new_partial_metadata_leaves_others_none() {
        let content = "---\ndescription: Partial\n---\nthe body";

        let skill = Skill::new("test", content);

        assert_eq!(skill.description(), "Partial");
        assert_eq!(skill.enabled_tools(), None);
        assert_eq!(skill.enabled_mcp_servers(), None);
        assert!(!skill.auto_unload());
        assert_eq!(skill.body(), "the body");
    }

    #[test]
    fn skill_new_ignores_unknown_keys() {
        let content = "---\ndescription: D\nbogus_field: 42\n---\nbody";

        let skill = Skill::new("test", content);

        assert_eq!(skill.description(), "D");
        assert_eq!(skill.body(), "body");
    }

    #[test]
    fn skill_new_trims_body_whitespace() {
        let content = "---\ndescription: D\n---\n\n\n  body content  \n\n";

        let skill = Skill::new("test", content);

        assert_eq!(skill.body(), "body content");
    }

    #[test]
    fn skill_default_has_empty_fields() {
        let skill = Skill::default();

        assert_eq!(skill.name(), "");
        assert_eq!(skill.body(), "");
        assert_eq!(skill.description(), "");
        assert_eq!(skill.enabled_tools(), None);
        assert_eq!(skill.enabled_mcp_servers(), None);
        assert!(!skill.auto_unload());
    }

    #[test]
    fn is_compatible_knowledge_only_passes_all_combinations() {
        let skill = Skill::new("test", "Just knowledge");

        assert!(skill.is_compatible(false, false));
        assert!(skill.is_compatible(true, false));
        assert!(skill.is_compatible(false, true));
        assert!(skill.is_compatible(true, true));
    }

    #[test]
    fn is_compatible_with_tools_requires_function_calling() {
        let content = "---\nenabled_tools: shell\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(!skill.is_compatible(false, true));
        assert!(!skill.is_compatible(false, false));
        assert!(skill.is_compatible(true, true));
        assert!(skill.is_compatible(true, false));
    }

    #[test]
    fn is_compatible_with_mcp_requires_mcp_enabled() {
        let content = "---\nenabled_mcp_servers: github\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(!skill.is_compatible(true, false));
        assert!(!skill.is_compatible(false, false));
        assert!(skill.is_compatible(true, true));
    }

    #[test]
    fn is_compatible_requires_both_when_both_declared() {
        let content =
            "---\nenabled_tools: shell\nenabled_mcp_servers: github\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(!skill.is_compatible(true, false));
        assert!(!skill.is_compatible(false, true));
        assert!(!skill.is_compatible(false, false));
        assert!(skill.is_compatible(true, true));
    }

    #[test]
    fn is_compatible_empty_string_tools_is_knowledge_only() {
        let content = "---\nenabled_tools: \"\"\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(skill.is_compatible(false, false));
    }

    #[test]
    fn builtin_returns_err_for_unknown_skill() {
        let result = Skill::builtin("nonexistent_skill_xyz");

        assert!(result.is_err());
    }
}
