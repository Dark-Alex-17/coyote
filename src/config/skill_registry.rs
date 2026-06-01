use super::role::{Role, RoleLike};
use super::skill::Skill;

use anyhow::{Result, bail};
use indexmap::IndexMap;
use std::collections::BTreeSet;

#[derive(Clone, Default)]
pub struct SkillRegistry {
    loaded: IndexMap<String, Skill>,
}

#[allow(dead_code)]
impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            loaded: IndexMap::new(),
        }
    }

    pub fn load(&mut self, name: &str) -> Result<()> {
        if self.loaded.contains_key(name) {
            bail!("Skill '{name}' is already loaded");
        }
        let skill = Skill::load(name)?;
        self.loaded.insert(name.to_string(), skill);

        Ok(())
    }

    pub fn insert(&mut self, skill: Skill) -> Result<()> {
        let name = skill.name().to_string();

        if self.loaded.contains_key(&name) {
            bail!("Skill '{name}' is already loaded");
        }

        self.loaded.insert(name, skill);

        Ok(())
    }

    pub fn unload(&mut self, name: &str) -> Result<()> {
        if self.loaded.shift_remove(name).is_none() {
            bail!("Skill '{name}' is not loaded");
        }

        Ok(())
    }

    pub fn loaded_names(&self) -> Vec<String> {
        self.loaded.keys().cloned().collect()
    }

    pub fn loaded_mcp_servers(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for skill in self.loaded.values() {
            if let Some(csv) = skill.enabled_mcp_servers() {
                for token in csv.split(',') {
                    let t = token.trim();
                    if !t.is_empty() {
                        out.insert(t.to_string());
                    }
                }
            }
        }
        out
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.contains_key(name)
    }

    pub fn sweep_auto_unload(&mut self) {
        self.loaded.retain(|_, skill| !skill.auto_unload());
    }

    pub fn effective_role(&self, base: &Role) -> Role {
        if self.loaded.is_empty() {
            return base.clone();
        }

        let mut effective = base.clone();
        let skip_body = effective.is_embedded_prompt();

        let base_tools_set = effective.enabled_tools().is_some();
        let base_mcps_set = effective.enabled_mcp_servers().is_some();

        let mut tools = parse_csv(effective.enabled_tools().as_deref());
        let mut mcps = parse_csv(effective.enabled_mcp_servers().as_deref());

        for (_, skill) in &self.loaded {
            tools.extend(parse_csv(skill.enabled_tools()));
            mcps.extend(parse_csv(skill.enabled_mcp_servers()));
            if !skip_body && !skill.body().is_empty() {
                let separator = if effective.is_empty_prompt() { "" } else { "\n\n" };
                effective.append_to_prompt(separator);
                effective.append_to_prompt(skill.body());
            }
        }

        if base_tools_set || !tools.is_empty() {
            effective.set_enabled_tools(Some(join_csv(&tools)));
        }

        if base_mcps_set || !mcps.is_empty() {
            effective.set_enabled_mcp_servers(Some(join_csv(&mcps)));
        }

        effective
    }
}

fn parse_csv(s: Option<&str>) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if let Some(raw) = s {
        for token in raw.split(',') {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                set.insert(trimmed.to_string());
            }
        }
    }
    set
}

fn join_csv(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(",")
}

#[cfg(test)]
impl SkillRegistry {
    fn insert_for_test(&mut self, skill: Skill) {
        self.loaded.insert(skill.name().to_string(), skill);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill(name: &str, frontmatter: &str, body: &str) -> Skill {
        let content = if frontmatter.is_empty() {
            body.to_string()
        } else {
            format!("---\n{frontmatter}\n---\n{body}")
        };
        Skill::new(name, &content)
    }

    #[test]
    fn empty_registry_returns_base_clone() {
        let base = Role::new("test", "You are a helper");
        let registry = SkillRegistry::new();

        let effective = registry.effective_role(&base);

        assert_eq!(effective.prompt(), base.prompt());
    }

    #[test]
    fn one_skill_appends_body_after_base_with_separator() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("git-master", "description: D", "Git knowledge"));

        let base = Role::new("test", "You are a helper");
        let effective = registry.effective_role(&base);

        assert_eq!(effective.prompt(), "You are a helper\n\nGit knowledge");
    }

    #[test]
    fn two_skills_compose_bodies_in_insertion_order() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("a", "", "Alpha body"));
        registry.insert_for_test(make_skill("b", "", "Beta body"));

        let base = Role::new("test", "Base");
        let effective = registry.effective_role(&base);

        assert_eq!(effective.prompt(), "Base\n\nAlpha body\n\nBeta body");
    }

    #[test]
    fn empty_base_prompt_omits_leading_separator() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("a", "", "Alpha"));
        registry.insert_for_test(make_skill("b", "", "Beta"));

        let base = Role::new("test", "");
        let effective = registry.effective_role(&base);

        assert_eq!(effective.prompt(), "Alpha\n\nBeta");
    }

    #[test]
    fn embedded_prompt_base_skips_body_composition() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill(
            "git-master",
            "enabled_tools: shell",
            "should not appear",
        ));

        let base = Role::new("test", "Process: __INPUT__");
        let effective = registry.effective_role(&base);

        assert_eq!(effective.prompt(), "Process: __INPUT__");
        let tools = effective.enabled_tools().expect("tools set by skill");
        assert!(tools.contains("shell"));
    }

    #[test]
    fn skills_with_empty_body_do_not_inject_separator() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("knowledge", "enabled_tools: fs", ""));

        let base = Role::new("test", "Base");
        let effective = registry.effective_role(&base);

        assert_eq!(effective.prompt(), "Base");
    }

    #[test]
    fn tools_and_mcps_are_unioned_and_deduplicated() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill(
            "a",
            "enabled_tools: shell,fs\nenabled_mcp_servers: github",
            "body",
        ));
        registry.insert_for_test(make_skill(
            "b",
            "enabled_tools: fs,git\nenabled_mcp_servers: github,jira",
            "body",
        ));

        let mut base = Role::new("test", "body");
        base.set_enabled_tools(Some("web_search".to_string()));

        let effective = registry.effective_role(&base);

        let tools_str = effective.enabled_tools().unwrap();
        let tools: BTreeSet<&str> = tools_str.split(',').collect();
        assert_eq!(
            tools,
            BTreeSet::from(["fs", "git", "shell", "web_search"])
        );

        let mcps_str = effective.enabled_mcp_servers().unwrap();
        let mcps: BTreeSet<&str> = mcps_str.split(',').collect();
        assert_eq!(mcps, BTreeSet::from(["github", "jira"]));
    }

    #[test]
    fn no_skill_tool_contributions_preserves_base_none() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("knowledge", "", "Pure knowledge"));

        let base = Role::new("test", "Base");
        let effective = registry.effective_role(&base);

        assert!(effective.enabled_tools().is_none());
        assert!(effective.enabled_mcp_servers().is_none());
    }

    #[test]
    fn base_some_empty_tools_is_preserved() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("knowledge", "", "Pure knowledge"));

        let mut base = Role::new("test", "Base");
        base.set_enabled_tools(Some(String::new()));
        let effective = registry.effective_role(&base);

        assert_eq!(effective.enabled_tools().as_deref(), Some(""));
    }

    #[test]
    fn load_already_loaded_returns_error() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("git-master", "", "body"));

        let err = registry.load("git-master").unwrap_err();

        assert!(err.to_string().contains("already loaded"));
    }

    #[test]
    fn unload_not_loaded_returns_error() {
        let mut registry = SkillRegistry::new();

        let err = registry.unload("missing").unwrap_err();

        assert!(err.to_string().contains("not loaded"));
    }

    #[test]
    fn unload_existing_succeeds_and_removes() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("git-master", "", "body"));
        assert!(registry.is_loaded("git-master"));

        registry.unload("git-master").unwrap();
        assert!(!registry.is_loaded("git-master"));
    }

    #[test]
    fn loaded_names_returns_insertion_order() {
        let mut registry = SkillRegistry::new();

        registry.insert_for_test(make_skill("zulu", "", "body"));
        registry.insert_for_test(make_skill("alpha", "", "body"));
        registry.insert_for_test(make_skill("mike", "", "body"));

        assert_eq!(
            registry.loaded_names(),
            vec!["zulu".to_string(), "alpha".to_string(), "mike".to_string()]
        );
    }

    #[test]
    fn sweep_removes_only_auto_unload_skills() {
        let mut registry = SkillRegistry::new();
        registry.insert_for_test(make_skill("ephemeral", "auto_unload: true", "body"));
        registry.insert_for_test(make_skill("persistent", "", "body"));

        registry.sweep_auto_unload();

        assert!(!registry.is_loaded("ephemeral"));
        assert!(registry.is_loaded("persistent"));
    }

    #[test]
    fn is_loaded_returns_false_for_unknown() {
        let registry = SkillRegistry::new();

        assert!(!registry.is_loaded("nothing"));
    }
}
