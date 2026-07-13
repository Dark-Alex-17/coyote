use super::*;

use anyhow::Result;
use fancy_regex::Regex;
use log::{debug, info};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Embed)]
#[folder = "assets/skills/"]
struct SkillsAsset;

static RE_METADATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)-{3,}\s*(.*?)\s*-{3,}\s*(.*)").unwrap());

pub const SKILL_SCAFFOLD: &str = "\
---
description: One-line description shown to the model when listing skills.
enabled_tools:
enabled_mcp_servers:
auto_unload: false
---
Replace this body with the knowledge or methodology this skill teaches.
";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Skill {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_mcp_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_unload: Option<bool>,
}

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
                        skill.enabled_tools = parse_skill_string_or_array(value);
                    }
                    "enabled_mcp_servers" => {
                        skill.enabled_mcp_servers = parse_skill_string_or_array(value);
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

    pub fn install_builtin_skills(force: bool) -> Result<()> {
        info!(
            "Installing built-in skills in {}",
            paths::skills_dir().display()
        );

        for file in SkillsAsset::iter() {
            debug!("Processing skill file: {}", file.as_ref());

            let embedded_file = SkillsAsset::get(&file)
                .ok_or_else(|| anyhow!("Failed to load embedded skill file: {}", file.as_ref()))?;
            let content = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
            let file_path = paths::skills_dir().join(file.as_ref());

            if file_path.exists() && !force {
                debug!(
                    "Skill file already exists, skipping: {}",
                    file_path.display()
                );
                continue;
            }

            ensure_parent_exists(&file_path)?;
            info!("Creating skill file: {}", file_path.display());
            let mut skill_file = File::create(&file_path)?;
            Write::write_all(&mut skill_file, content.as_bytes())?;
        }

        Ok(())
    }

    pub fn load(name: &str) -> Result<Self> {
        paths::validate_skill_name(name)?;
        let path = if paths::workspace_skill_file(name).is_file() {
            paths::workspace_skill_file(name)
        } else {
            paths::skill_file(name)
        };
        let content = read_to_string(&path)
            .with_context(|| format!("Failed to read skill '{name}' at {}", path.display()))?;
        Ok(Skill::new(name, &content))
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

    pub fn enabled_tools(&self) -> Option<&[String]> {
        self.enabled_tools.as_deref()
    }

    pub fn enabled_mcp_servers(&self) -> Option<&[String]> {
        self.enabled_mcp_servers.as_deref()
    }

    pub fn auto_unload(&self) -> bool {
        self.auto_unload.unwrap_or(false)
    }

    pub fn is_compatible(&self, mcp_enabled: bool) -> bool {
        if self.declares_mcp_servers() && !mcp_enabled {
            return false;
        }

        true
    }

    fn declares_mcp_servers(&self) -> bool {
        self.enabled_mcp_servers
            .as_deref()
            .map(|servers| !servers.is_empty())
            .unwrap_or(false)
    }
}

fn parse_skill_string_or_array(value: &Value) -> Option<Vec<String>> {
    if value.is_null() {
        return None;
    }
    if let Some(s) = value.as_str() {
        return Some(csv_to_vec(s));
    }
    if let Some(arr) = value.as_array() {
        let items: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        return Some(items);
    }
    None
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
        assert_eq!(
            skill.enabled_tools(),
            Some(["shell".to_string(), "fs".to_string()].as_slice())
        );
        assert_eq!(
            skill.enabled_mcp_servers(),
            Some(["github".to_string()].as_slice())
        );
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
    fn is_compatible_knowledge_only_passes_both_mcp_states() {
        let skill = Skill::new("test", "Just knowledge");

        assert!(skill.is_compatible(false));
        assert!(skill.is_compatible(true));
    }

    #[test]
    fn is_compatible_with_tools_only_passes_both_mcp_states() {
        let content = "---\nenabled_tools: shell\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(skill.is_compatible(false));
        assert!(skill.is_compatible(true));
    }

    #[test]
    fn is_compatible_with_mcp_requires_mcp_enabled() {
        let content = "---\nenabled_mcp_servers: github\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(!skill.is_compatible(false));
        assert!(skill.is_compatible(true));
    }

    #[test]
    fn is_compatible_with_both_requires_mcp_enabled() {
        let content = "---\nenabled_tools: shell\nenabled_mcp_servers: github\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(!skill.is_compatible(false));
        assert!(skill.is_compatible(true));
    }

    #[test]
    fn is_compatible_empty_string_mcps_is_knowledge_only() {
        let content = "---\nenabled_mcp_servers: \"\"\n---\nbody";

        let skill = Skill::new("test", content);

        assert!(skill.is_compatible(false));
    }
}
