use crate::mcp::McpServersConfig;

use fancy_regex::Regex;
use indexmap::IndexMap;
use log::warn;
use std::collections::HashMap;
use std::fmt;

/// The configuration level that contributed a layer of tool patterns for an
/// MCP server, as rendered in diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource {
    Global,
    AppConfig,
    Role(String),
    Agent(String),
    Session,
    Skill(String),
    Node(String),
}

impl fmt::Display for LayerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayerSource::Global => write!(f, "global (mcp.json)"),
            LayerSource::AppConfig => write!(f, "config (config.yaml)"),
            LayerSource::Role(name) => write!(f, "role ({name})"),
            LayerSource::Agent(name) => write!(f, "agent ({name})"),
            LayerSource::Session => write!(f, "session (.set)"),
            LayerSource::Skill(name) => write!(f, "skill ({name})"),
            LayerSource::Node(id) => write!(f, "node ({id})"),
        }
    }
}

impl LayerSource {
    pub fn short_label(&self) -> &'static str {
        match self {
            LayerSource::Global => "global",
            LayerSource::AppConfig => "config",
            LayerSource::Role(_) => "role",
            LayerSource::Agent(_) => "agent",
            LayerSource::Session => "session",
            LayerSource::Skill(_) => "skill",
            LayerSource::Node(_) => "node",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompiledPatterns {
    source: LayerSource,
    raw: Vec<String>,
    regexes: Vec<Regex>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolFilter {
    layers: Vec<CompiledPatterns>,
}

impl ToolFilter {
    pub fn push_layer(&mut self, source: LayerSource, patterns: &[String]) {
        self.layers.push(CompiledPatterns {
            source,
            raw: patterns.to_vec(),
            regexes: patterns.iter().map(|p| compile_glob(p)).collect(),
        });
    }

    pub fn layers(&self) -> impl Iterator<Item = (&LayerSource, &[String])> {
        self.layers
            .iter()
            .map(|layer| (&layer.source, layer.raw.as_slice()))
    }

    pub fn allows(&self, tool: &str) -> bool {
        self.layers.iter().all(|layer| {
            layer
                .regexes
                .iter()
                .any(|regex| regex.is_match(tool).unwrap_or(false))
        })
    }

    /// The first matching raw pattern per layer, in layer order, or the
    /// source of the first layer with no match.
    pub fn allows_explain(&self, tool: &str) -> Result<Vec<(&LayerSource, &str)>, &LayerSource> {
        let mut matched = Vec::with_capacity(self.layers.len());
        for layer in &self.layers {
            // fancy_regex can fail at match time (backtracking limits);
            // treat that as a non-match rather than allowing the tool.
            match layer
                .regexes
                .iter()
                .position(|regex| regex.is_match(tool).unwrap_or(false))
            {
                Some(index) => matched.push((&layer.source, layer.raw[index].as_str())),
                None => return Err(&layer.source),
            }
        }
        Ok(matched)
    }

    pub fn dead_context_patterns(&self, advertised: &[String]) -> Vec<(&LayerSource, &str)> {
        let surviving: Vec<&String> = advertised
            .iter()
            .filter(|name| {
                self.layers
                    .iter()
                    .filter(|layer| layer.source == LayerSource::Global)
                    .all(|layer| {
                        layer
                            .regexes
                            .iter()
                            .any(|regex| regex.is_match(name).unwrap_or(false))
                    })
            })
            .collect();
        let mut dead = Vec::new();
        for layer in self
            .layers
            .iter()
            .filter(|l| l.source != LayerSource::Global)
        {
            for (raw, regex) in layer.raw.iter().zip(&layer.regexes) {
                if !surviving
                    .iter()
                    .any(|name| regex.is_match(name).unwrap_or(false))
                {
                    dead.push((&layer.source, raw.as_str()));
                }
            }
        }

        dead
    }
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
        warn!("Invalid MCP tool pattern '{pattern}': {error}. It will match nothing.");
        never_matching_regex()
    })
}

fn never_matching_regex() -> Regex {
    Regex::new("(?!)").expect("'(?!)' is a valid never-matching regex")
}

pub struct SkillMcpLayer {
    pub name: String,
    pub enabled_servers: Vec<String>,
    pub mcp_tools: IndexMap<String, Vec<String>>,
}

pub struct McpToolPolicy;

impl McpToolPolicy {
    #[allow(clippy::too_many_arguments)]
    pub fn effective(
        mcp_config: &McpServersConfig,
        session: Option<&IndexMap<String, Vec<String>>>,
        agent: Option<(&str, &IndexMap<String, Vec<String>>)>,
        role: Option<(&str, &IndexMap<String, Vec<String>>)>,
        global: Option<&IndexMap<String, Vec<String>>>,
        skills: &[SkillMcpLayer],
        node: Option<(&str, &IndexMap<String, Vec<String>>)>,
        aliases: &IndexMap<String, String>,
    ) -> HashMap<String, ToolFilter> {
        let mut filters: HashMap<String, ToolFilter> = HashMap::new();

        for (server, spec) in &mcp_config.mcp_servers {
            if let Some(patterns) = &spec.allowed_tools {
                filters
                    .entry(server.clone())
                    .or_default()
                    .push_layer(LayerSource::Global, patterns);
            }
        }

        if let Some(map) = global {
            push_level(
                &mut filters,
                mcp_config,
                aliases,
                &LayerSource::AppConfig,
                map,
                None,
            );
        }
        if let Some((name, map)) = role {
            push_level(
                &mut filters,
                mcp_config,
                aliases,
                &LayerSource::Role(name.to_string()),
                map,
                None,
            );
        }
        if let Some((name, map)) = agent {
            push_level(
                &mut filters,
                mcp_config,
                aliases,
                &LayerSource::Agent(name.to_string()),
                map,
                None,
            );
        }
        if let Some(map) = session {
            push_level(
                &mut filters,
                mcp_config,
                aliases,
                &LayerSource::Session,
                map,
                None,
            );
        }
        for skill in skills {
            push_level(
                &mut filters,
                mcp_config,
                aliases,
                &LayerSource::Skill(skill.name.clone()),
                &skill.mcp_tools,
                Some(&skill.enabled_servers),
            );
        }
        if let Some((id, map)) = node {
            push_level(
                &mut filters,
                mcp_config,
                aliases,
                &LayerSource::Node(id.to_string()),
                map,
                None,
            );
        }

        filters
    }
}

fn push_level(
    filters: &mut HashMap<String, ToolFilter>,
    mcp_config: &McpServersConfig,
    aliases: &IndexMap<String, String>,
    source: &LayerSource,
    map: &IndexMap<String, Vec<String>>,
    enabled_servers: Option<&[String]>,
) {
    for (server, patterns) in expand_server_keys(mcp_config, aliases, map) {
        if let Some(enabled) = enabled_servers
            && !enabled.iter().any(|id| id == &server)
        {
            continue;
        }
        filters
            .entry(server)
            .or_default()
            .push_layer(source.clone(), &patterns);
    }
}

fn expand_server_keys(
    mcp_config: &McpServersConfig,
    aliases: &IndexMap<String, String>,
    map: &IndexMap<String, Vec<String>>,
) -> IndexMap<String, Vec<String>> {
    let mut expanded: IndexMap<String, Vec<String>> = IndexMap::new();
    for (key, patterns) in map {
        let key = key.trim();
        if mcp_config.mcp_servers.contains_key(key) {
            expanded
                .entry(key.to_string())
                .or_default()
                .extend(patterns.iter().cloned());
        } else {
            for mapped_id in expand_mcp_server_alias(aliases, key) {
                if mcp_config.mcp_servers.contains_key(&mapped_id) {
                    expanded
                        .entry(mapped_id)
                        .or_default()
                        .extend(patterns.iter().cloned());
                }
            }
        }
    }

    expanded
}

pub(crate) fn expand_mcp_server_alias(
    aliases: &IndexMap<String, String>,
    key: &str,
) -> Vec<String> {
    aliases
        .get(key)
        .map(|mapped| {
            mapped
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{McpServer, McpServersConfig, McpTransportType};

    fn spec(allowed_tools: Option<&[&str]>) -> McpServer {
        McpServer {
            transport_type: McpTransportType::Stdio,
            command: Some("echo".to_string()),
            args: None,
            env: None,
            cwd: None,
            url: None,
            headers: None,
            oauth: None,
            allowed_tools: allowed_tools.map(list),
        }
    }

    fn config(servers: &[(&str, Option<&[&str]>)]) -> McpServersConfig {
        McpServersConfig {
            mcp_servers: servers
                .iter()
                .map(|(name, tools)| (name.to_string(), spec(*tools)))
                .collect(),
        }
    }

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn tool_map(entries: &[(&str, &[&str])]) -> IndexMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(server, patterns)| (server.to_string(), list(patterns)))
            .collect()
    }

    fn single_layer(patterns: &[&str]) -> ToolFilter {
        layered(&[(LayerSource::Global, patterns)])
    }

    fn layered(layers: &[(LayerSource, &[&str])]) -> ToolFilter {
        let mut filter = ToolFilter::default();
        for (source, patterns) in layers {
            filter.push_layer(source.clone(), &list(patterns));
        }
        filter
    }

    fn no_aliases() -> IndexMap<String, String> {
        IndexMap::new()
    }

    fn aliases(entries: &[(&str, &str)]) -> IndexMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn resolve(
        config: &McpServersConfig,
        session: Option<&IndexMap<String, Vec<String>>>,
        role: Option<(&str, &IndexMap<String, Vec<String>>)>,
    ) -> HashMap<String, ToolFilter> {
        McpToolPolicy::effective(config, session, None, role, None, &[], None, &no_aliases())
    }

    #[test]
    fn literal_pattern_matches_only_the_exact_name() {
        let filter = single_layer(&["get_issue"]);

        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("get_issues"));
        assert!(!filter.allows("get_issu"));
        assert!(!filter.allows("xget_issue"));
    }

    #[test]
    fn star_matches_any_run_of_characters() {
        let filter = single_layer(&["get_*"]);
        assert!(filter.allows("get_issue"));
        assert!(filter.allows("get_"));
        assert!(!filter.allows("set_issue"));

        let filter = single_layer(&["*_issue"]);
        assert!(filter.allows("create_issue"));
        assert!(!filter.allows("create_pr"));

        let filter = single_layer(&["get*sue"]);
        assert!(filter.allows("get_issue"));
        assert!(filter.allows("getsue"));

        let filter = single_layer(&["*"]);
        assert!(filter.allows(""));
        assert!(filter.allows("anything_at_all"));
    }

    #[test]
    fn question_mark_matches_exactly_one_character() {
        let filter = single_layer(&["get_?"]);

        assert!(filter.allows("get_a"));
        assert!(!filter.allows("get_"));
        assert!(!filter.allows("get_ab"));
    }

    #[test]
    fn regex_metacharacters_are_matched_literally() {
        let filter = single_layer(&["get.issue"]);
        assert!(filter.allows("get.issue"));
        assert!(!filter.allows("getXissue"));

        for pattern in ["a(b", "a[b", "a+b", "a|b", "a$b"] {
            let filter = single_layer(&[pattern]);
            assert!(filter.allows(pattern), "'{pattern}' should match itself");
            assert!(!filter.allows("ab"), "'{pattern}' should not match 'ab'");
        }
    }

    #[test]
    fn backslash_is_literal_and_star_still_wildcards() {
        let filter = single_layer(&["a\\b"]);
        assert!(filter.allows("a\\b"));
        assert!(!filter.allows("ab"));

        let filter = single_layer(&["a\\*b"]);
        assert!(filter.allows("a\\b"));
        assert!(filter.allows("a\\xyzb"));
        assert!(!filter.allows("ab"));
    }

    #[test]
    fn the_never_matching_placeholder_matches_nothing() {
        let regex = never_matching_regex();

        assert!(!regex.is_match("").unwrap());
        assert!(!regex.is_match("anything").unwrap());
    }

    #[test]
    fn within_a_layer_any_pattern_may_match() {
        let filter = single_layer(&["get_*", "set_*"]);

        assert!(filter.allows("get_x"));
        assert!(filter.allows("set_x"));
        assert!(!filter.allows("delete_x"));
    }

    #[test]
    fn across_layers_every_layer_must_match() {
        let filter = layered(&[
            (LayerSource::Global, &["get_*"]),
            (LayerSource::Session, &["*_issue"]),
        ]);

        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("get_pr"));
        assert!(!filter.allows("create_issue"));
    }

    #[test]
    fn an_empty_layer_blocks_everything() {
        let filter = layered(&[(LayerSource::Global, &["*"]), (LayerSource::Session, &[])]);

        assert!(!filter.allows("anything"));
        assert_eq!(
            filter.allows_explain("anything"),
            Err(&LayerSource::Session)
        );
    }

    #[test]
    fn allows_explain_reports_the_first_matching_pattern_per_layer() {
        let filter = layered(&[
            (LayerSource::Global, &["x_*", "get_*"]),
            (LayerSource::Session, &["*"]),
        ]);

        assert_eq!(
            filter.allows_explain("get_issue").unwrap(),
            vec![
                (&LayerSource::Global, "get_*"),
                (&LayerSource::Session, "*")
            ]
        );
    }

    #[test]
    fn allows_explain_reports_the_first_layer_without_a_match() {
        let filter = layered(&[
            (LayerSource::Global, &["get_*"]),
            (LayerSource::Session, &["*"]),
        ]);
        assert_eq!(
            filter.allows_explain("delete_repo"),
            Err(&LayerSource::Global)
        );

        let filter = layered(&[
            (LayerSource::Global, &["*"]),
            (LayerSource::Session, &["get_*"]),
        ]);
        assert_eq!(
            filter.allows_explain("delete_repo"),
            Err(&LayerSource::Session)
        );
    }

    #[test]
    fn global_allowed_tools_from_mcp_json_is_the_first_layer() {
        let config = config(&[("gh", Some(&["get_*"]))]);
        let session_map = tool_map(&[("gh", &["*"])]);

        let filters = resolve(&config, Some(&session_map), None);

        assert_eq!(
            filters["gh"].allows_explain("get_issue").unwrap(),
            vec![
                (&LayerSource::Global, "get_*"),
                (&LayerSource::Session, "*")
            ]
        );
    }

    #[test]
    fn servers_without_patterns_at_any_level_are_absent() {
        let config = config(&[("gh", None)]);

        let filters = resolve(&config, None, None);

        assert!(filters.is_empty());
    }

    #[test]
    fn server_absent_from_a_level_map_gets_no_layer_from_it() {
        let config = config(&[("gh", Some(&["get_*"])), ("gl", None)]);
        let role_map = tool_map(&[("gl", &["x_*"])]);

        let filters = resolve(&config, None, Some(("dev", &role_map)));

        assert!(filters["gh"].allows("get_issue"));
        assert!(!filters["gh"].allows("delete_repo"));
        assert_eq!(filters["gh"].allows_explain("get_issue").unwrap().len(), 1);
        assert!(filters["gl"].allows("x_1"));
        assert!(!filters["gl"].allows("y_1"));
    }

    #[test]
    fn empty_pattern_list_at_a_level_blocks_all_tools_for_that_server() {
        let config = config(&[("gh", Some(&["get_*"]))]);
        let session_map = tool_map(&[("gh", &[])]);

        let filters = resolve(&config, Some(&session_map), None);

        assert!(!filters["gh"].allows("get_issue"));
        assert_eq!(
            filters["gh"].allows_explain("get_issue"),
            Err(&LayerSource::Session)
        );
    }

    #[test]
    fn session_cannot_widen_a_role_restriction() {
        let config = config(&[("gh", None)]);
        let role_map = tool_map(&[("gh", &["get_*"])]);
        let session_map = tool_map(&[("gh", &["*"])]);

        let filters = resolve(&config, Some(&session_map), Some(("dev", &role_map)));

        assert!(filters["gh"].allows("get_issue"));
        assert!(!filters["gh"].allows("delete_repo"));
    }

    #[test]
    fn app_config_map_contributes_its_own_layer() {
        let config = config(&[("gh", None)]);
        let app_map = tool_map(&[("gh", &["get_*"])]);

        let filters = McpToolPolicy::effective(
            &config,
            None,
            None,
            None,
            Some(&app_map),
            &[],
            None,
            &no_aliases(),
        );

        assert_eq!(
            filters["gh"].allows_explain("get_issue").unwrap(),
            vec![(&LayerSource::AppConfig, "get_*")]
        );
        assert!(!filters["gh"].allows("delete_repo"));
    }

    #[test]
    fn skill_layer_applies_only_to_its_enabled_servers() {
        let config = config(&[("gh", None), ("gl", None)]);
        let skill = SkillMcpLayer {
            name: "reviewer".to_string(),
            enabled_servers: vec!["gh".to_string()],
            mcp_tools: tool_map(&[("gh", &["get_*"]), ("gl", &["*"])]),
        };

        let filters = McpToolPolicy::effective(
            &config,
            None,
            None,
            None,
            None,
            &[skill],
            None,
            &no_aliases(),
        );

        assert!(filters.contains_key("gh"));
        assert!(!filters.contains_key("gl"));
    }

    #[test]
    fn two_skills_naming_the_same_server_stack_independent_layers() {
        let config = config(&[("gh", None)]);
        let skills = vec![
            SkillMcpLayer {
                name: "a".to_string(),
                enabled_servers: vec!["gh".to_string()],
                mcp_tools: tool_map(&[("gh", &["get_*"])]),
            },
            SkillMcpLayer {
                name: "b".to_string(),
                enabled_servers: vec!["gh".to_string()],
                mcp_tools: tool_map(&[("gh", &["*_issue"])]),
            },
        ];

        let filters = McpToolPolicy::effective(
            &config,
            None,
            None,
            None,
            None,
            &skills,
            None,
            &no_aliases(),
        );

        assert!(filters["gh"].allows("get_issue"));
        assert!(!filters["gh"].allows("get_pr"));
        assert!(!filters["gh"].allows("create_issue"));
        assert_eq!(filters["gh"].allows_explain("get_issue").unwrap().len(), 2);
    }

    #[test]
    fn layers_stack_in_documented_order_with_node_last() {
        let config = config(&[("gh", Some(&["*"]))]);
        let app_map = tool_map(&[("gh", &["*"])]);
        let role_map = tool_map(&[("gh", &["*"])]);
        let agent_map = tool_map(&[("gh", &["*"])]);
        let session_map = tool_map(&[("gh", &["*"])]);
        let skills = vec![SkillMcpLayer {
            name: "reviewer".to_string(),
            enabled_servers: vec!["gh".to_string()],
            mcp_tools: tool_map(&[("gh", &["*"])]),
        }];
        let node_map = tool_map(&[("gh", &["*"])]);

        let filters = McpToolPolicy::effective(
            &config,
            Some(&session_map),
            Some(("worker", &agent_map)),
            Some(("dev", &role_map)),
            Some(&app_map),
            &skills,
            Some(("n1", &node_map)),
            &no_aliases(),
        );

        let sources: Vec<String> = filters["gh"]
            .allows_explain("anything")
            .unwrap()
            .iter()
            .map(|(source, _)| source.to_string())
            .collect();
        assert_eq!(
            sources,
            vec![
                "global (mcp.json)",
                "config (config.yaml)",
                "role (dev)",
                "agent (worker)",
                "session (.set)",
                "skill (reviewer)",
                "node (n1)",
            ]
        );
    }

    #[test]
    fn alias_key_expands_to_all_mapped_servers() {
        let config = config(&[("github", None), ("gitlab", None)]);
        let role_map = tool_map(&[("gh", &["get_*"])]);

        let filters = McpToolPolicy::effective(
            &config,
            None,
            None,
            Some(("dev", &role_map)),
            None,
            &[],
            None,
            &aliases(&[("gh", "github,gitlab")]),
        );

        assert!(filters["github"].allows("get_issue"));
        assert!(!filters["github"].allows("delete_repo"));
        assert!(filters["gitlab"].allows("get_issue"));
        assert!(!filters["gitlab"].allows("delete_repo"));
    }

    #[test]
    fn alias_ids_missing_from_the_config_are_skipped() {
        let config = config(&[("github", None)]);
        let role_map = tool_map(&[("gh", &["get_*"])]);

        let filters = McpToolPolicy::effective(
            &config,
            None,
            None,
            Some(("dev", &role_map)),
            None,
            &[],
            None,
            &aliases(&[("gh", "github,missing")]),
        );

        assert_eq!(filters.len(), 1);
        assert!(filters.contains_key("github"));
    }

    #[test]
    fn unknown_map_keys_are_dropped() {
        let config = config(&[("github", None)]);
        let role_map = tool_map(&[("nope", &["get_*"])]);

        let filters = resolve(&config, None, Some(("dev", &role_map)));

        assert!(filters.is_empty());
    }

    #[test]
    fn alias_and_direct_key_for_the_same_server_merge_into_one_layer() {
        let config = config(&[("github", None)]);
        let role_map = tool_map(&[("gh", &["get_*"]), ("github", &["set_*"])]);

        let filters = McpToolPolicy::effective(
            &config,
            None,
            None,
            Some(("dev", &role_map)),
            None,
            &[],
            None,
            &aliases(&[("gh", "github")]),
        );

        assert!(filters["github"].allows("get_issue"));
        assert!(filters["github"].allows("set_topic"));
        assert!(!filters["github"].allows("delete_repo"));
        assert_eq!(
            filters["github"].allows_explain("get_issue").unwrap().len(),
            1
        );
    }

    #[test]
    fn layer_source_display() {
        assert_eq!(LayerSource::Global.to_string(), "global (mcp.json)");
        assert_eq!(LayerSource::AppConfig.to_string(), "config (config.yaml)");
        assert_eq!(LayerSource::Role("dev".into()).to_string(), "role (dev)");
        assert_eq!(
            LayerSource::Agent("worker".into()).to_string(),
            "agent (worker)"
        );
        assert_eq!(LayerSource::Session.to_string(), "session (.set)");
        assert_eq!(
            LayerSource::Skill("review".into()).to_string(),
            "skill (review)"
        );
        assert_eq!(LayerSource::Node("n1".into()).to_string(), "node (n1)");
    }

    #[test]
    fn dead_context_patterns_flags_patterns_matching_nothing() {
        let filter = layered(&[
            (LayerSource::Global, &["get_*"]),
            (LayerSource::Role("dev".into()), &["get_issue", "set_*"]),
        ]);

        let advertised = vec!["get_issue".to_string(), "set_topic".to_string()];
        let dead = filter.dead_context_patterns(&advertised);

        // set_* only matches set_topic, which the global layer hides.
        assert_eq!(dead, vec![(&LayerSource::Role("dev".into()), "set_*")]);
    }

    #[test]
    fn dead_context_patterns_is_empty_when_every_pattern_is_live() {
        let filter = layered(&[
            (LayerSource::Global, &["get_*"]),
            (LayerSource::Session, &["get_issue"]),
        ]);

        let advertised = vec!["get_issue".to_string()];
        assert!(filter.dead_context_patterns(&advertised).is_empty());
    }

    #[test]
    fn dead_context_patterns_ignores_the_global_layer_itself() {
        let filter = layered(&[(LayerSource::Global, &["zzz_*"])]);

        let advertised = vec!["get_issue".to_string()];
        assert!(filter.dead_context_patterns(&advertised).is_empty());
    }

    #[test]
    fn expand_mcp_server_alias_splits_and_trims() {
        let aliases = aliases(&[("gh", "github, gitlab,")]);

        assert_eq!(
            expand_mcp_server_alias(&aliases, "gh"),
            vec!["github".to_string(), "gitlab".to_string()]
        );
        assert!(expand_mcp_server_alias(&aliases, "nope").is_empty());
    }
}
