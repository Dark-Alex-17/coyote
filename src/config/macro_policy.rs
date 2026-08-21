use super::agent::Agent;
use super::app_config::AppConfig;
use super::macros::Macro;
use super::paths;
use super::role::Role;
use super::session::Session;

use log::warn;
use std::collections::HashSet;
use std::fmt;
use std::fs::{read_dir, read_to_string};
use std::path::PathBuf;

pub const RESERVED_MACRO_NAMES: [&str; 2] = ["enable", "disable"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroSource {
    Workspace,
    Global,
}

impl fmt::Display for MacroSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MacroSource::Workspace => write!(f, "workspace"),
            MacroSource::Global => write!(f, "global"),
        }
    }
}

/// The configuration level whose `enabled_macros` allowlist won the
/// first-`Some`-wins precedence chain (session > agent > role > global).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroAllowlistLevel {
    Session,
    Agent,
    Role,
    Global,
}

impl fmt::Display for MacroAllowlistLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MacroAllowlistLevel::Session => write!(f, "session"),
            MacroAllowlistLevel::Agent => write!(f, "agent"),
            MacroAllowlistLevel::Role => write!(f, "role"),
            MacroAllowlistLevel::Global => write!(f, "global"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroState {
    Enabled,
    DisabledRuntime,
    Locked { level: MacroAllowlistLevel },
    Missing,
    ShadowedBuiltin,
    Invalid { reason: String },
}

impl MacroState {
    pub fn is_invocable(&self) -> bool {
        matches!(self, MacroState::Enabled | MacroState::ShadowedBuiltin)
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredMacro {
    pub name: String,
    pub source: MacroSource,
    pub definition: Result<Macro, String>,
    pub shadowed_by_workspace: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedMacro {
    pub name: String,
    pub source: Option<MacroSource>,
    pub description: Option<String>,
    pub isolated: Option<bool>,
    pub shadowed_by_workspace: bool,
    pub state: MacroState,
}

#[derive(Debug)]
pub struct MacroPolicy {
    pub macros: Vec<ResolvedMacro>,
}

impl MacroPolicy {
    pub fn effective(
        global: &AppConfig,
        role: Option<&Role>,
        agent: Option<&Agent>,
        session: Option<&Session>,
        builtin_commands: &[&str],
        no_workspace_macros: bool,
    ) -> Self {
        Self::effective_with(
            discover_macros(no_workspace_macros),
            session.and_then(|s| s.enabled_macros()),
            agent.and_then(|a| a.enabled_macros()),
            role.and_then(|r| r.enabled_macros()),
            global.enabled_macros.as_deref(),
            builtin_commands,
        )
    }

    fn effective_with(
        discovered: Vec<DiscoveredMacro>,
        session_list: Option<&[String]>,
        agent_list: Option<&[String]>,
        role_list: Option<&[String]>,
        global_list: Option<&[String]>,
        builtin_commands: &[&str],
    ) -> Self {
        let allowlist = session_list
            .map(|list| (MacroAllowlistLevel::Session, list))
            .or_else(|| agent_list.map(|list| (MacroAllowlistLevel::Agent, list)))
            .or_else(|| role_list.map(|list| (MacroAllowlistLevel::Role, list)))
            .or_else(|| global_list.map(|list| (MacroAllowlistLevel::Global, list)));

        let mut macros: Vec<ResolvedMacro> = discovered
            .into_iter()
            .map(|discovered_macro| {
                let state = resolve_state(&discovered_macro, allowlist, builtin_commands);
                let (description, isolated) = match &discovered_macro.definition {
                    Ok(value) => (value.description.clone(), Some(value.isolated)),
                    Err(_) => (None, None),
                };
                ResolvedMacro {
                    name: discovered_macro.name,
                    source: Some(discovered_macro.source),
                    description,
                    isolated,
                    shadowed_by_workspace: discovered_macro.shadowed_by_workspace,
                    state,
                }
            })
            .collect();

        if let Some((_, list)) = allowlist {
            let known: HashSet<&str> = macros.iter().map(|m| m.name.as_str()).collect();
            let mut missing: Vec<ResolvedMacro> = vec![];
            for name in list {
                if !known.contains(name.as_str()) && !missing.iter().any(|m| &m.name == name) {
                    warn!("enabled_macros references macro '{name}' which is not installed");
                    missing.push(ResolvedMacro {
                        name: name.clone(),
                        source: None,
                        description: None,
                        isolated: None,
                        shadowed_by_workspace: false,
                        state: MacroState::Missing,
                    });
                }
            }

            macros.extend(missing);
        }

        macros.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| source_rank(a.source).cmp(&source_rank(b.source)))
        });

        Self { macros }
    }

    pub fn find(&self, name: &str) -> Option<&ResolvedMacro> {
        self.macros
            .iter()
            .find(|m| m.name == name && m.source.is_some() && !m.shadowed_by_workspace)
    }
}

fn source_rank(source: Option<MacroSource>) -> u8 {
    match source {
        Some(MacroSource::Workspace) => 0,
        Some(MacroSource::Global) => 1,
        None => 2,
    }
}

fn resolve_state(
    discovered: &DiscoveredMacro,
    allowlist: Option<(MacroAllowlistLevel, &[String])>,
    builtin_commands: &[&str],
) -> MacroState {
    if RESERVED_MACRO_NAMES.contains(&discovered.name.as_str()) {
        warn!(
            "Ignoring macro '{}': the name is reserved for '.macro {}'",
            discovered.name, discovered.name
        );

        return MacroState::Invalid {
            reason: format!("'{}' is a reserved macro name", discovered.name),
        };
    }

    if let Err(reason) = &discovered.definition {
        return MacroState::Invalid {
            reason: reason.clone(),
        };
    }

    if let Some((level, list)) = allowlist
        && !list.iter().any(|name| name == &discovered.name)
    {
        return match level {
            MacroAllowlistLevel::Global => MacroState::DisabledRuntime,
            level => MacroState::Locked { level },
        };
    }

    if builtin_commands.contains(&discovered.name.as_str()) {
        return MacroState::ShadowedBuiltin;
    }

    MacroState::Enabled
}

pub fn discover_macros(no_workspace_macros: bool) -> Vec<DiscoveredMacro> {
    let mut dirs = vec![];
    if !no_workspace_macros {
        dirs.push((MacroSource::Workspace, paths::workspace_macros_dir()));
    }

    dirs.push((MacroSource::Global, paths::macros_dir()));
    discover_macros_in(&dirs)
}

fn discover_macros_in(dirs: &[(MacroSource, PathBuf)]) -> Vec<DiscoveredMacro> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut output = vec![];

    for (source, dir) in dirs {
        let Ok(rd) = read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let is_file = entry
                .file_type()
                .map(|file_type| file_type.is_file())
                .unwrap_or(false);

            if !is_file {
                continue;
            }

            let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|v| v.strip_suffix(".yaml"))
                .map(str::to_string)
            else {
                continue;
            };

            if name.is_empty() {
                continue;
            }

            let definition = read_to_string(entry.path())
                .map_err(|err| err.to_string())
                .and_then(|content| {
                    serde_yaml::from_str::<Macro>(&content).map_err(|err| err.to_string())
                });
            let shadowed_by_workspace = !seen.insert(name.clone());
            output.push(DiscoveredMacro {
                name,
                source: *source,
                definition,
                shadowed_by_workspace,
            });
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::get_env_name;
    use serial_test::serial;
    use std::path::Path;
    use std::{env, fs, time};

    fn valid_macro() -> Macro {
        Macro {
            description: Some("a test macro".to_string()),
            isolated: true,
            variables: vec![],
            steps: vec![".help".to_string()],
        }
    }

    fn disc(name: &str, source: MacroSource) -> DiscoveredMacro {
        DiscoveredMacro {
            name: name.to_string(),
            source,
            definition: Ok(valid_macro()),
            shadowed_by_workspace: false,
        }
    }

    fn disc_invalid(name: &str, reason: &str) -> DiscoveredMacro {
        DiscoveredMacro {
            name: name.to_string(),
            source: MacroSource::Global,
            definition: Err(reason.to_string()),
            shadowed_by_workspace: false,
        }
    }

    fn globals(names: &[&str]) -> Vec<DiscoveredMacro> {
        names.iter().map(|n| disc(n, MacroSource::Global)).collect()
    }

    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn resolve(
        discovered: Vec<DiscoveredMacro>,
        session: Option<&[String]>,
        agent: Option<&[String]>,
        role: Option<&[String]>,
        global: Option<&[String]>,
    ) -> MacroPolicy {
        MacroPolicy::effective_with(discovered, session, agent, role, global, &[])
    }

    fn state_of<'a>(policy: &'a MacroPolicy, name: &str) -> &'a MacroState {
        &policy
            .macros
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("no row for macro '{name}'"))
            .state
    }

    #[test]
    fn all_none_enables_everything() {
        let policy = resolve(globals(&["a", "b"]), None, None, None, None);

        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
        assert_eq!(state_of(&policy, "b"), &MacroState::Enabled);
    }

    #[test]
    fn global_empty_list_disables_all_as_runtime() {
        let l = list(&[]);
        let policy = resolve(globals(&["a", "b"]), None, None, None, Some(&l));

        assert_eq!(state_of(&policy, "a"), &MacroState::DisabledRuntime);
        assert_eq!(state_of(&policy, "b"), &MacroState::DisabledRuntime);
    }

    #[test]
    fn global_populated_partitions_enabled_and_disabled_runtime() {
        let l = list(&["a"]);
        let policy = resolve(globals(&["a", "b"]), None, None, None, Some(&l));

        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
        assert_eq!(state_of(&policy, "b"), &MacroState::DisabledRuntime);
    }

    #[test]
    fn role_populated_locks_excluded_at_role_level() {
        let l = list(&["a"]);

        let policy = resolve(globals(&["a", "b"]), None, None, Some(&l), None);

        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
        assert_eq!(
            state_of(&policy, "b"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Role
            }
        );
    }

    #[test]
    fn agent_populated_locks_excluded_at_agent_level() {
        let l = list(&["a"]);

        let policy = resolve(globals(&["a", "b"]), None, Some(&l), None, None);

        assert_eq!(
            state_of(&policy, "b"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Agent
            }
        );
    }

    #[test]
    fn session_populated_locks_excluded_at_session_level() {
        let l = list(&["a"]);

        let policy = resolve(globals(&["a", "b"]), Some(&l), None, None, None);

        assert_eq!(
            state_of(&policy, "b"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Session
            }
        );
    }

    #[test]
    fn role_empty_list_locks_everything_at_role_level() {
        let l = list(&[]);

        let policy = resolve(globals(&["a"]), None, None, Some(&l), None);

        assert_eq!(
            state_of(&policy, "a"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Role
            }
        );
    }

    #[test]
    fn agent_empty_list_locks_everything_at_agent_level() {
        let l = list(&[]);

        let policy = resolve(globals(&["a"]), None, Some(&l), None, None);

        assert_eq!(
            state_of(&policy, "a"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Agent
            }
        );
    }

    #[test]
    fn session_empty_list_locks_everything_at_session_level() {
        let l = list(&[]);

        let policy = resolve(globals(&["a"]), Some(&l), None, None, None);

        assert_eq!(
            state_of(&policy, "a"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Session
            }
        );
    }

    #[test]
    fn session_wins_over_agent() {
        let session = list(&["a"]);
        let agent = list(&["b"]);

        let policy = resolve(
            globals(&["a", "b"]),
            Some(&session),
            Some(&agent),
            None,
            None,
        );

        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
        assert_eq!(
            state_of(&policy, "b"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Session
            }
        );
    }

    #[test]
    fn agent_wins_over_role() {
        let agent = list(&["a"]);
        let role = list(&["b"]);

        let policy = resolve(globals(&["a", "b"]), None, Some(&agent), Some(&role), None);

        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
        assert_eq!(
            state_of(&policy, "b"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Agent
            }
        );
    }

    #[test]
    fn role_wins_over_global() {
        let role = list(&["a"]);
        let global = list(&["b"]);

        let policy = resolve(globals(&["a", "b"]), None, None, Some(&role), Some(&global));

        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
        assert_eq!(
            state_of(&policy, "b"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Role
            }
        );
    }

    #[test]
    fn empty_list_at_session_beats_populated_global() {
        let session = list(&[]);
        let global = list(&["a"]);

        let policy = resolve(globals(&["a"]), Some(&session), None, None, Some(&global));

        assert_eq!(
            state_of(&policy, "a"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Session
            }
        );
    }

    #[test]
    fn unknown_allowlist_name_yields_missing_row_without_error() {
        let l = list(&["a", "ghost"]);

        let policy = resolve(globals(&["a"]), None, None, None, Some(&l));

        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
        assert_eq!(state_of(&policy, "ghost"), &MacroState::Missing);
        let ghost = policy.macros.iter().find(|m| m.name == "ghost").unwrap();
        assert_eq!(ghost.source, None);
    }

    #[test]
    fn missing_row_deduplicated_for_repeated_allowlist_names() {
        let l = list(&["ghost", "ghost"]);

        let policy = resolve(vec![], None, None, None, Some(&l));

        assert_eq!(policy.macros.len(), 1);
        assert_eq!(state_of(&policy, "ghost"), &MacroState::Missing);
    }

    #[test]
    fn no_missing_rows_without_an_allowlist() {
        let policy = resolve(globals(&["a"]), None, None, None, None);

        assert_eq!(policy.macros.len(), 1);
    }

    #[test]
    fn builtin_name_collision_is_shadowed() {
        let policy =
            MacroPolicy::effective_with(globals(&["help", "a"]), None, None, None, None, &["help"]);

        assert_eq!(state_of(&policy, "help"), &MacroState::ShadowedBuiltin);
        assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
    }

    #[test]
    fn locked_wins_over_shadowed_builtin() {
        let l = list(&["a"]);

        let policy = MacroPolicy::effective_with(
            globals(&["help", "a"]),
            Some(&l),
            None,
            None,
            None,
            &["help"],
        );

        assert_eq!(
            state_of(&policy, "help"),
            &MacroState::Locked {
                level: MacroAllowlistLevel::Session
            }
        );
    }

    #[test]
    fn allowlisted_builtin_collision_stays_shadowed() {
        let l = list(&["help"]);

        let policy =
            MacroPolicy::effective_with(globals(&["help"]), None, None, None, Some(&l), &["help"]);

        assert_eq!(state_of(&policy, "help"), &MacroState::ShadowedBuiltin);
    }

    #[test]
    fn reserved_names_are_invalid() {
        let policy = resolve(globals(&["enable", "disable"]), None, None, None, None);
        for name in RESERVED_MACRO_NAMES {
            assert_eq!(
                state_of(&policy, name),
                &MacroState::Invalid {
                    reason: format!("'{name}' is a reserved macro name")
                }
            );
        }
    }

    #[test]
    fn reserved_name_invalid_even_when_allowlisted() {
        let l = list(&["enable"]);

        let policy = resolve(globals(&["enable"]), Some(&l), None, None, None);

        assert_eq!(
            state_of(&policy, "enable"),
            &MacroState::Invalid {
                reason: "'enable' is a reserved macro name".to_string()
            }
        );
    }

    #[test]
    fn reserved_name_invalid_wins_over_builtin_collision() {
        let policy =
            MacroPolicy::effective_with(globals(&["enable"]), None, None, None, None, &["enable"]);

        assert_eq!(
            state_of(&policy, "enable"),
            &MacroState::Invalid {
                reason: "'enable' is a reserved macro name".to_string()
            }
        );
    }

    #[test]
    fn parse_failure_is_invalid() {
        let policy = resolve(vec![disc_invalid("bad", "boom")], None, None, None, None);

        assert_eq!(
            state_of(&policy, "bad"),
            &MacroState::Invalid {
                reason: "boom".to_string()
            }
        );
        let bad = policy.macros.iter().find(|m| m.name == "bad").unwrap();
        assert_eq!(bad.description, None);
        assert_eq!(bad.isolated, None);
    }

    #[test]
    fn invalid_wins_over_allowlist_exclusion() {
        let l = list(&["other"]);

        let policy = resolve(
            vec![disc_invalid("bad", "boom")],
            Some(&l),
            None,
            None,
            None,
        );

        assert_eq!(
            state_of(&policy, "bad"),
            &MacroState::Invalid {
                reason: "boom".to_string()
            }
        );
    }

    #[test]
    fn workspace_shadowing_keeps_both_rows_and_find_returns_workspace() {
        let discovered = vec![
            disc("a", MacroSource::Workspace),
            DiscoveredMacro {
                shadowed_by_workspace: true,
                ..disc("a", MacroSource::Global)
            },
        ];

        let policy = resolve(discovered, None, None, None, None);

        assert_eq!(policy.macros.len(), 2);
        assert_eq!(policy.macros[0].source, Some(MacroSource::Workspace));
        assert!(!policy.macros[0].shadowed_by_workspace);
        assert_eq!(policy.macros[1].source, Some(MacroSource::Global));
        assert!(policy.macros[1].shadowed_by_workspace);
        let found = policy.find("a").unwrap();
        assert_eq!(found.source, Some(MacroSource::Workspace));
    }

    #[test]
    fn find_skips_missing_rows() {
        let l = list(&["ghost"]);

        let policy = resolve(vec![], None, None, None, Some(&l));

        assert!(policy.find("ghost").is_none());
    }

    #[test]
    fn rows_are_sorted_by_name() {
        let policy = resolve(globals(&["c", "a", "b"]), None, None, None, None);

        let names: Vec<&str> = policy.macros.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn resolved_rows_carry_description_and_isolated() {
        let policy = resolve(globals(&["a"]), None, None, None, None);

        let row = policy.macros.first().unwrap();
        assert_eq!(row.description.as_deref(), Some("a test macro"));
        assert_eq!(row.isolated, Some(true));
    }

    #[test]
    fn is_invocable_only_for_enabled_and_shadowed() {
        assert!(MacroState::Enabled.is_invocable());
        assert!(MacroState::ShadowedBuiltin.is_invocable());
        assert!(!MacroState::DisabledRuntime.is_invocable());
        assert!(
            !MacroState::Locked {
                level: MacroAllowlistLevel::Role
            }
            .is_invocable()
        );
        assert!(!MacroState::Missing.is_invocable());
        assert!(
            !MacroState::Invalid {
                reason: "x".to_string()
            }
            .is_invocable()
        );
    }

    #[test]
    fn level_and_source_display() {
        assert_eq!(MacroAllowlistLevel::Session.to_string(), "session");
        assert_eq!(MacroAllowlistLevel::Agent.to_string(), "agent");
        assert_eq!(MacroAllowlistLevel::Role.to_string(), "role");
        assert_eq!(MacroAllowlistLevel::Global.to_string(), "global");
        assert_eq!(MacroSource::Workspace.to_string(), "workspace");
        assert_eq!(MacroSource::Global.to_string(), "global");
    }

    fn with_macro_dirs<F: FnOnce(&Path, &Path)>(f: F) {
        let unique = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-macro-policy-test-{unique}"));
        let workspace = root.join("workspace-macros");
        let global = root.join("global-macros");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&global).unwrap();
        f(&workspace, &global);
        let _ = fs::remove_dir_all(&root);
    }

    fn write_macro(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(format!("{name}.yaml")), content).unwrap();
    }

    const VALID_YAML: &str = "steps:\n  - \".help\"\n";

    #[test]
    fn discovery_scans_workspace_then_global_with_shadowing() {
        with_macro_dirs(|workspace, global| {
            write_macro(workspace, "both", VALID_YAML);
            write_macro(workspace, "ws-only", VALID_YAML);
            write_macro(global, "both", VALID_YAML);
            write_macro(global, "global-only", VALID_YAML);

            let discovered = discover_macros_in(&[
                (MacroSource::Workspace, workspace.to_path_buf()),
                (MacroSource::Global, global.to_path_buf()),
            ]);

            assert_eq!(discovered.len(), 4);
            let both_ws = discovered
                .iter()
                .find(|d| d.name == "both" && d.source == MacroSource::Workspace)
                .unwrap();
            assert!(!both_ws.shadowed_by_workspace);
            let both_global = discovered
                .iter()
                .find(|d| d.name == "both" && d.source == MacroSource::Global)
                .unwrap();
            assert!(both_global.shadowed_by_workspace);
            let global_only = discovered.iter().find(|d| d.name == "global-only").unwrap();
            assert!(!global_only.shadowed_by_workspace);
        });
    }

    #[test]
    fn discovery_ignores_non_yaml_files_and_directories() {
        with_macro_dirs(|_, global| {
            write_macro(global, "good", VALID_YAML);
            fs::write(global.join("notes.txt"), "not a macro").unwrap();
            fs::write(global.join(".yaml"), VALID_YAML).unwrap();
            fs::create_dir_all(global.join("subdir.yaml")).unwrap();

            let discovered = discover_macros_in(&[(MacroSource::Global, global.to_path_buf())]);

            assert_eq!(discovered.len(), 1);
            assert_eq!(discovered[0].name, "good");
        });
    }

    #[test]
    fn discovery_records_parse_failures() {
        with_macro_dirs(|_, global| {
            write_macro(global, "broken", "steps: {not valid");

            let discovered = discover_macros_in(&[(MacroSource::Global, global.to_path_buf())]);

            assert_eq!(discovered.len(), 1);
            assert!(discovered[0].definition.is_err());
        });
    }

    #[test]
    fn discovery_of_nonexistent_dirs_is_empty() {
        let discovered = discover_macros_in(&[(
            MacroSource::Global,
            PathBuf::from("/nonexistent/coyote-macro-policy-test"),
        )]);
        assert!(discovered.is_empty());
    }

    fn with_macro_dir_envs<F: FnOnce()>(workspace: &Path, global: &Path, f: F) {
        let ws_env = get_env_name("workspace_config_dir");
        let global_env = get_env_name("macros_dir");
        let prev_ws = env::var_os(&ws_env);
        let prev_global = env::var_os(&global_env);
        unsafe {
            env::set_var(&ws_env, workspace);
            env::set_var(&global_env, global);
        }
        f();
        unsafe {
            match prev_ws {
                Some(v) => env::set_var(&ws_env, v),
                None => env::remove_var(&ws_env),
            }
            match prev_global {
                Some(v) => env::set_var(&global_env, v),
                None => env::remove_var(&global_env),
            }
        }
    }

    #[test]
    #[serial]
    fn discover_macros_honors_no_workspace_macros() {
        with_macro_dirs(|workspace_root, global| {
            let macros_subdir = workspace_root.join("macros");
            fs::create_dir_all(&macros_subdir).unwrap();
            write_macro(&macros_subdir, "ws-macro", VALID_YAML);
            write_macro(global, "global-macro", VALID_YAML);

            with_macro_dir_envs(workspace_root, global, || {
                let with_workspace = discover_macros(false);
                let names: Vec<&str> = with_workspace.iter().map(|d| d.name.as_str()).collect();
                assert!(names.contains(&"ws-macro"));
                assert!(names.contains(&"global-macro"));

                let without_workspace = discover_macros(true);
                let names: Vec<&str> = without_workspace.iter().map(|d| d.name.as_str()).collect();
                assert!(!names.contains(&"ws-macro"));
                assert!(names.contains(&"global-macro"));
            });
        });
    }

    #[test]
    #[serial]
    fn effective_honors_no_workspace_macros() {
        with_macro_dirs(|workspace_root, global| {
            let macros_subdir = workspace_root.join("macros");
            fs::create_dir_all(&macros_subdir).unwrap();
            write_macro(&macros_subdir, "shared", VALID_YAML);
            write_macro(&macros_subdir, "ws-only", VALID_YAML);
            write_macro(global, "shared", VALID_YAML);
            write_macro(global, "global-only", VALID_YAML);

            with_macro_dir_envs(workspace_root, global, || {
                let config = AppConfig::default();

                let policy = MacroPolicy::effective(&config, None, None, None, &[], false);
                assert_eq!(
                    policy.find("shared").unwrap().source,
                    Some(MacroSource::Workspace)
                );
                assert!(policy.find("ws-only").is_some());
                assert!(policy.find("global-only").is_some());

                let policy = MacroPolicy::effective(&config, None, None, None, &[], true);
                assert_eq!(
                    policy.find("shared").unwrap().source,
                    Some(MacroSource::Global)
                );
                assert!(policy.find("ws-only").is_none());
                assert!(policy.find("global-only").is_some());
            });
        });
    }

    #[test]
    #[serial]
    fn effective_resolves_role_session_and_global_levels() {
        with_macro_dirs(|workspace_root, global_dir| {
            write_macro(global_dir, "a", VALID_YAML);
            write_macro(global_dir, "b", VALID_YAML);

            with_macro_dir_envs(workspace_root, global_dir, || {
                let global = AppConfig {
                    enabled_macros: Some(vec!["a".to_string()]),
                    ..AppConfig::default()
                };

                let policy = MacroPolicy::effective(&global, None, None, None, &[], false);
                assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
                assert_eq!(state_of(&policy, "b"), &MacroState::DisabledRuntime);

                let role = Role::new("test", "---\nenabled_macros: b\n---\nbody");
                let policy = MacroPolicy::effective(&global, Some(&role), None, None, &[], false);
                assert_eq!(
                    state_of(&policy, "a"),
                    &MacroState::Locked {
                        level: MacroAllowlistLevel::Role
                    }
                );
                assert_eq!(state_of(&policy, "b"), &MacroState::Enabled);

                let session: Session = serde_yaml::from_str(
                    "model: provider:test\nenabled_macros: \"a\"\nmessages: []",
                )
                .unwrap();
                let policy =
                    MacroPolicy::effective(&global, Some(&role), None, Some(&session), &[], false);
                assert_eq!(state_of(&policy, "a"), &MacroState::Enabled);
                assert_eq!(
                    state_of(&policy, "b"),
                    &MacroState::Locked {
                        level: MacroAllowlistLevel::Session
                    }
                );
            });
        });
    }

    #[test]
    #[serial]
    fn effective_pins_empty_string_role_allowlist_as_explicit_zero() {
        with_macro_dirs(|workspace_root, global_dir| {
            write_macro(global_dir, "a", VALID_YAML);

            with_macro_dir_envs(workspace_root, global_dir, || {
                let global = AppConfig {
                    enabled_macros: Some(vec!["a".to_string()]),
                    ..AppConfig::default()
                };
                let role = Role::new("test", "---\nenabled_macros: \"\"\n---\nbody");

                let policy = MacroPolicy::effective(&global, Some(&role), None, None, &[], false);
                assert_eq!(
                    state_of(&policy, "a"),
                    &MacroState::Locked {
                        level: MacroAllowlistLevel::Role
                    }
                );
            });
        });
    }
}
