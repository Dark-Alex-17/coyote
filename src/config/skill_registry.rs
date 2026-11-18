use super::mcp_tool_policy::expand_mcp_server_alias;
use super::role::{Role, RoleLike};
use super::skill::Skill;
use super::skill_policy::SkillPolicy;
use super::{Agent, AppConfig};

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashSet};

/// Holds a skill's grants to what the active agent declares: a skill must never
/// give the model tools or MCP servers the agent definition does not.
pub struct AgentToolGate<'a> {
    agent_name: &'a str,
    declared_tools: HashSet<String>,
    mapping_tools: &'a IndexMap<String, String>,
    /// `None` when the agent declares `all`.
    declared_mcp_servers: Option<BTreeSet<String>>,
    mapping_mcp_servers: &'a IndexMap<String, String>,
}

impl<'a> AgentToolGate<'a> {
    pub fn new(agent: &'a Agent, app: &'a AppConfig) -> Self {
        let names = agent.mcp_server_names();
        let declared_mcp_servers = (!names.iter().any(|entry| entry.trim() == "all")).then(|| {
            let mut servers = BTreeSet::new();
            for entry in names.iter().map(|entry| entry.trim()) {
                let members = expand_mcp_server_alias(&app.mapping_mcp_servers, entry);
                if members.is_empty() {
                    servers.insert(entry.to_string());
                } else {
                    servers.extend(members);
                }
            }
            servers
        });
        Self {
            agent_name: agent.name(),
            declared_tools: agent
                .functions()
                .declarations()
                .iter()
                .map(|declaration| declaration.name.clone())
                .collect(),
            mapping_tools: &app.mapping_tools,
            declared_mcp_servers,
            mapping_mcp_servers: &app.mapping_mcp_servers,
        }
    }

    pub fn partition(&self, granted: &[String]) -> (Vec<String>, Vec<String>) {
        let mut allowed = vec![];
        let mut denied = vec![];
        for entry in granted.iter().map(|entry| entry.trim()) {
            if entry.is_empty() {
                continue;
            }
            if entry == "all" {
                let mut declared: Vec<String> = self.declared_tools.iter().cloned().collect();
                declared.sort();
                allowed.extend(declared);
                continue;
            }
            let members: Vec<&str> = match self.mapping_tools.get(entry) {
                Some(values) => values.split(',').map(str::trim).collect(),
                None => vec![entry],
            };
            for tool in members.into_iter().filter(|tool| !tool.is_empty()) {
                let bucket = if self.declared_tools.contains(tool) {
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

    pub fn allows_mcp_server(&self, server: &str) -> bool {
        self.declared_mcp_servers
            .as_ref()
            .is_none_or(|servers| servers.contains(server))
    }

    pub fn partition_mcp_servers(&self, granted: &[String]) -> (Vec<String>, Vec<String>) {
        let mut allowed = vec![];
        let mut denied = vec![];
        for entry in granted.iter().map(|entry| entry.trim()) {
            if entry.is_empty() {
                continue;
            }
            if entry == "all" {
                match &self.declared_mcp_servers {
                    Some(servers) => allowed.extend(servers.iter().cloned()),
                    None => allowed.push(entry.to_string()),
                }
                continue;
            }
            let mut members = expand_mcp_server_alias(self.mapping_mcp_servers, entry);
            if members.is_empty() {
                members.push(entry.to_string());
            }
            for server in members {
                let bucket = if self.allows_mcp_server(&server) {
                    &mut allowed
                } else {
                    &mut denied
                };
                if !bucket.contains(&server) {
                    bucket.push(server);
                }
            }
        }
        (allowed, denied)
    }

    pub fn warnings(&self, skill: &Skill) -> Vec<String> {
        let (_, denied) = self.partition(skill.enabled_tools().unwrap_or_default());
        let (_, denied_servers) =
            self.partition_mcp_servers(skill.enabled_mcp_servers().unwrap_or_default());
        let tool_warnings = denied
            .iter()
            .map(|tool| not_enabled_warning(skill.name(), tool, self.agent_name));
        let server_warnings = denied_servers
            .iter()
            .map(|server| mcp_not_enabled_warning(skill.name(), server, self.agent_name));
        tool_warnings.chain(server_warnings).collect()
    }
}

pub fn not_enabled_warning(skill: &str, tool: &str, agent: &str) -> String {
    format!(
        "Skill '{skill}' grants tool '{tool}', but it is not enabled for agent '{agent}' and will not be offered. \
         To use it with this skill, add {tool} to {agent}'s global_tools in the agent definition."
    )
}

pub fn mcp_not_enabled_warning(skill: &str, server: &str, agent: &str) -> String {
    format!(
        "Skill '{skill}' grants MCP server '{server}', but it is not enabled for agent '{agent}' and will not be offered. \
         To use it with this skill, add {server} to {agent}'s mcp_servers in the agent definition."
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

    pub fn loaded_mcp_servers(&self, agent_gate: Option<&AgentToolGate>) -> BTreeSet<String> {
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
        match agent_gate {
            Some(gate) => {
                let granted: Vec<String> = out.into_iter().collect();
                gate.partition_mcp_servers(&granted).0.into_iter().collect()
            }
            None => out,
        }
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.contains_key(name)
    }

    pub fn sweep_auto_unload(&mut self) {
        self.loaded.retain(|_, skill| !skill.auto_unload());
    }

    pub fn effective_role(
        &self,
        base: &Role,
        policy: &SkillPolicy,
        agent_gate: Option<&AgentToolGate>,
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
                match agent_gate {
                    Some(gate) => tools.extend(gate.partition(skill_tools).0),
                    None => tools.extend(skill_tools.iter().cloned()),
                }
            }
            if let Some(servers) = skill.enabled_mcp_servers() {
                match agent_gate {
                    Some(gate) => mcps.extend(gate.partition_mcp_servers(servers).0),
                    None => mcps.extend(servers.iter().cloned()),
                }
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
    fn agent_gate_holds_skill_mcp_servers_to_the_agents_declaration() {
        let app = AppConfig {
            mapping_mcp_servers: IndexMap::from([("gh".to_string(), "github, gitlab".to_string())]),
            ..AppConfig::default()
        };
        let agent = Agent::test_new(super::super::AgentConfig {
            name: "gated".into(),
            mcp_servers: vec!["gh".into()],
            ..Default::default()
        });
        let gate = AgentToolGate::new(&agent, &app);

        let granted = ["github", "slack"].map(String::from);
        assert_eq!(
            gate.partition_mcp_servers(&granted),
            (vec!["github".to_string()], vec!["slack".to_string()])
        );
        assert_eq!(
            gate.partition_mcp_servers(&["all".to_string()]),
            (vec!["github".to_string(), "gitlab".to_string()], vec![])
        );

        let open = Agent::test_new(super::super::AgentConfig {
            name: "open".into(),
            mcp_servers: vec!["all".into()],
            ..Default::default()
        });
        assert!(AgentToolGate::new(&open, &app).allows_mcp_server("slack"));
    }

    #[test]
    fn agent_gate_expands_aliases_and_all_before_splitting() {
        let agent = agent_providing(&["fs_read", "fs_ls"]);
        let app = AppConfig {
            mapping_tools: IndexMap::from([("fs".to_string(), "fs_read, fs_write".to_string())]),
            ..AppConfig::default()
        };
        let gate = AgentToolGate::new(&agent, &app);

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
        let app = AppConfig::default();
        let gate = AgentToolGate::new(&agent, &app);
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
