use super::Agent;
use super::role::{Role, RoleLike};
use super::skill::Skill;
use super::skill_policy::SkillPolicy;

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashSet};

/// The tools an active agent provides. A skill loaded inside an agent only
/// brings into reach the granted tools the agent already provides; it never
/// widens the agent's `global_tools`.
pub struct AgentToolGate<'a> {
    agent_name: &'a str,
    provided: HashSet<String>,
    mapping_tools: &'a IndexMap<String, String>,
}

impl<'a> AgentToolGate<'a> {
    pub fn new(agent: &'a Agent, mapping_tools: &'a IndexMap<String, String>) -> Self {
        Self {
            agent_name: agent.name(),
            provided: agent
                .functions()
                .declarations()
                .iter()
                .map(|declaration| declaration.name.clone())
                .collect(),
            mapping_tools,
        }
    }

    /// Splits a skill's granted entries into the tool names the agent
    /// provides and the ones it does not. A `mapping_tools` alias expands to
    /// its members and `all` to every tool the agent provides.
    pub fn partition(&self, granted: &[String]) -> (Vec<String>, Vec<String>) {
        let mut allowed = vec![];
        let mut denied = vec![];
        for entry in granted.iter().map(|entry| entry.trim()) {
            if entry.is_empty() {
                continue;
            }
            if entry == "all" {
                let mut provided: Vec<String> = self.provided.iter().cloned().collect();
                provided.sort();
                allowed.extend(provided);
                continue;
            }
            let members: Vec<&str> = match self.mapping_tools.get(entry) {
                Some(values) => values.split(',').map(str::trim).collect(),
                None => vec![entry],
            };
            for tool in members.into_iter().filter(|tool| !tool.is_empty()) {
                let bucket = if self.provided.contains(tool) {
                    &mut allowed
                } else {
                    &mut denied
                };
                if !bucket.iter().any(|seen| seen == tool) {
                    bucket.push(tool.to_string());
                }
            }
        }
        (allowed, denied)
    }

    /// One warning per granted tool the agent does not provide.
    pub fn warnings(&self, skill: &Skill) -> Vec<String> {
        let (_, denied) = self.partition(skill.enabled_tools().unwrap_or_default());
        denied
            .iter()
            .map(|tool| not_enabled_warning(skill.name(), tool, self.agent_name))
            .collect()
    }
}

pub fn not_enabled_warning(skill: &str, tool: &str, agent: &str) -> String {
    format!(
        "Skill '{skill}' grants tool '{tool}', but it is not enabled for agent '{agent}' and will not be offered. \
         To use it with this skill, add {tool} to {agent}'s global_tools in the agent definition."
    )
}

#[derive(Clone, Default)]
pub struct SkillRegistry {
    loaded: IndexMap<String, Skill>,
}

impl SkillRegistry {
    pub fn insert(&mut self, skill: Skill) -> Result<()> {
        let name = skill.name().to_string();

        if self.loaded.contains_key(&name) {
            bail!("Skill '{name}' is already loaded");
        }

        self.loaded.insert(name, skill);

        Ok(())
    }

    pub fn unload(&mut self, name: &str) -> Result<Skill> {
        self.loaded
            .shift_remove(name)
            .ok_or_else(|| anyhow!("Skill '{name}' is not loaded"))
    }

    pub fn loaded_names(&self) -> Vec<String> {
        self.loaded.keys().cloned().collect()
    }

    pub fn loaded_skills(&self) -> impl Iterator<Item = &Skill> {
        self.loaded.values()
    }

    pub fn loaded_mcp_servers(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for skill in self.loaded.values() {
            if let Some(servers) = skill.enabled_mcp_servers() {
                for token in servers {
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

    /// The base role with every loaded skill composed in. Inside an agent,
    /// `agent_tools` keeps a skill's granted tools to the ones the agent
    /// provides; with no agent the grants apply as written.
    pub fn effective_role(
        &self,
        base: &Role,
        policy: &SkillPolicy,
        agent_tools: Option<&AgentToolGate>,
    ) -> Role {
        if !policy.skills_enabled || self.loaded.is_empty() {
            return base.clone();
        }

        let mut effective = base.clone();
        let skip_body = effective.is_embedded_prompt();

        let base_tools = effective.enabled_tools();
        let base_tools_set = base_tools.is_some();
        let base_mcps = effective.enabled_mcp_servers();
        let base_mcps_set = base_mcps.is_some();

        let mut tools: BTreeSet<String> = base_tools.unwrap_or_default().into_iter().collect();
        let mut mcps: BTreeSet<String> = base_mcps.unwrap_or_default().into_iter().collect();

        for (name, skill) in &self.loaded {
            if !policy.allows(name) {
                continue;
            }
            if let Some(skill_tools) = skill.enabled_tools() {
                match agent_tools {
                    Some(gate) => tools.extend(gate.partition(skill_tools).0),
                    None => tools.extend(skill_tools.iter().cloned()),
                }
            }
            if let Some(servers) = skill.enabled_mcp_servers() {
                mcps.extend(servers.iter().cloned());
            }
            if !skip_body && !skill.body().is_empty() {
                let separator = if effective.is_empty_prompt() {
                    ""
                } else {
                    "\n\n"
                };
                effective.append_to_prompt(separator);
                effective.append_to_prompt(skill.body());
            }
        }

        if base_tools_set || !tools.is_empty() {
            effective.set_enabled_tools(Some(tools.into_iter().collect()));
        }

        if base_mcps_set || !mcps.is_empty() {
            effective.set_enabled_mcp_servers(Some(mcps.into_iter().collect()));
        }

        effective
    }
}

#[cfg(test)]
impl SkillRegistry {
    fn insert_for_test(&mut self, skill: Skill) {
        self.loaded.insert(skill.name().to_string(), skill);
    }

    fn effective_role_for_test(&self, base: &Role) -> Role {
        let policy = SkillPolicy {
            skills_enabled: true,
            enabled: self.loaded.keys().cloned().collect(),
            compatible_enabled: self.loaded.keys().cloned().collect(),
        };
        self.effective_role(base, &policy, None)
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
        let registry = SkillRegistry::default();

        let effective = registry.effective_role_for_test(&base);

        assert_eq!(effective.prompt(), base.prompt());
    }

    #[test]
    fn one_skill_appends_body_after_base_with_separator() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("git-master", "description: D", "Git knowledge"));

        let base = Role::new("test", "You are a helper");
        let effective = registry.effective_role_for_test(&base);

        assert_eq!(effective.prompt(), "You are a helper\n\nGit knowledge");
    }

    #[test]
    fn two_skills_compose_bodies_in_insertion_order() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("a", "", "Alpha body"));
        registry.insert_for_test(make_skill("b", "", "Beta body"));

        let base = Role::new("test", "Base");
        let effective = registry.effective_role_for_test(&base);

        assert_eq!(effective.prompt(), "Base\n\nAlpha body\n\nBeta body");
    }

    #[test]
    fn empty_base_prompt_omits_leading_separator() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("a", "", "Alpha"));
        registry.insert_for_test(make_skill("b", "", "Beta"));

        let base = Role::new("test", "");
        let effective = registry.effective_role_for_test(&base);

        assert_eq!(effective.prompt(), "Alpha\n\nBeta");
    }

    #[test]
    fn embedded_prompt_base_skips_body_composition() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill(
            "git-master",
            "enabled_tools: shell",
            "should not appear",
        ));

        let base = Role::new("test", "Process: __INPUT__");
        let effective = registry.effective_role_for_test(&base);

        assert_eq!(effective.prompt(), "Process: __INPUT__");
        let tools = effective.enabled_tools().expect("tools set by skill");
        assert!(tools.iter().any(|s| s == "shell"));
    }

    #[test]
    fn skills_with_empty_body_do_not_inject_separator() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("knowledge", "enabled_tools: fs", ""));

        let base = Role::new("test", "Base");
        let effective = registry.effective_role_for_test(&base);

        assert_eq!(effective.prompt(), "Base");
    }

    #[test]
    fn tools_and_mcps_are_unioned_and_deduplicated() {
        let mut registry = SkillRegistry::default();
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
        base.set_enabled_tools(Some(vec!["web_search".to_string()]));

        let effective = registry.effective_role_for_test(&base);

        let tools_vec = effective.enabled_tools().unwrap();
        let tools: BTreeSet<&str> = tools_vec.iter().map(|s| s.as_str()).collect();
        assert_eq!(tools, BTreeSet::from(["fs", "git", "shell", "web_search"]));

        let mcps_vec = effective.enabled_mcp_servers().unwrap();
        let mcps: BTreeSet<&str> = mcps_vec.iter().map(|s| s.as_str()).collect();
        assert_eq!(mcps, BTreeSet::from(["github", "jira"]));
    }

    #[test]
    fn no_skill_tool_contributions_preserves_base_none() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("knowledge", "", "Pure knowledge"));

        let base = Role::new("test", "Base");
        let effective = registry.effective_role_for_test(&base);

        assert!(effective.enabled_tools().is_none());
        assert!(effective.enabled_mcp_servers().is_none());
    }

    #[test]
    fn base_some_empty_tools_is_preserved() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("knowledge", "", "Pure knowledge"));

        let mut base = Role::new("test", "Base");
        base.set_enabled_tools(Some(Vec::new()));
        let effective = registry.effective_role_for_test(&base);

        assert_eq!(effective.enabled_tools().as_deref(), Some([].as_slice()));
    }

    #[test]
    fn unload_not_loaded_returns_error() {
        let mut registry = SkillRegistry::default();

        let err = registry.unload("missing").unwrap_err();

        assert!(err.to_string().contains("not loaded"));
    }

    #[test]
    fn unload_existing_succeeds_and_removes() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("git-master", "", "body"));
        assert!(registry.is_loaded("git-master"));

        registry.unload("git-master").unwrap();
        assert!(!registry.is_loaded("git-master"));
    }

    #[test]
    fn loaded_names_returns_insertion_order() {
        let mut registry = SkillRegistry::default();

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
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill("ephemeral", "auto_unload: true", "body"));
        registry.insert_for_test(make_skill("persistent", "", "body"));

        registry.sweep_auto_unload();

        assert!(!registry.is_loaded("ephemeral"));
        assert!(registry.is_loaded("persistent"));
    }

    #[test]
    fn is_loaded_returns_false_for_unknown() {
        let registry = SkillRegistry::default();

        assert!(!registry.is_loaded("nothing"));
    }

    fn agent_providing(tools: &[&str]) -> Agent {
        let mut agent = Agent::test_new(super::super::AgentConfig {
            name: "gated".into(),
            ..Default::default()
        });
        for tool in tools {
            agent
                .functions_mut()
                .append_declaration(crate::function::FunctionDeclaration {
                    name: tool.to_string(),
                    description: String::new(),
                    parameters: Default::default(),
                    agent: false,
                });
        }
        agent
    }

    #[test]
    fn agent_gate_expands_aliases_and_all_before_splitting() {
        let agent = agent_providing(&["fs_read", "fs_ls"]);
        let mapping = IndexMap::from([("fs".to_string(), "fs_read, fs_write".to_string())]);
        let gate = AgentToolGate::new(&agent, &mapping);

        let granted = ["fs", "execute_command", "fs_read"].map(String::from);
        assert_eq!(
            gate.partition(&granted),
            (
                vec!["fs_read".to_string()],
                vec!["fs_write".to_string(), "execute_command".to_string()]
            )
        );
        assert_eq!(
            gate.partition(&["all".to_string()]),
            (vec!["fs_ls".to_string(), "fs_read".to_string()], vec![])
        );
    }

    #[test]
    fn effective_role_inside_an_agent_keeps_only_the_grants_the_agent_provides() {
        let mut registry = SkillRegistry::default();
        registry.insert_for_test(make_skill(
            "grants-exec",
            "enabled_tools: fs_read, execute_command",
            "body",
        ));
        let agent = agent_providing(&["fs_read"]);
        let mapping = IndexMap::new();
        let gate = AgentToolGate::new(&agent, &mapping);
        let policy = SkillPolicy {
            skills_enabled: true,
            enabled: registry.loaded.keys().cloned().collect(),
            compatible_enabled: registry.loaded.keys().cloned().collect(),
        };

        let effective = registry.effective_role(&Role::new("r", "p"), &policy, Some(&gate));

        assert_eq!(effective.enabled_tools(), Some(vec!["fs_read".to_string()]));
        assert_eq!(
            gate.warnings(registry.loaded.get("grants-exec").unwrap()),
            vec![not_enabled_warning(
                "grants-exec",
                "execute_command",
                "gated"
            )]
        );
    }
}
