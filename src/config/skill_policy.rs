use super::agent::Agent;
use super::app_config::AppConfig;
use super::paths;
use super::role::Role;
use super::session::Session;
use super::skill::Skill;

use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeSet, HashSet};

#[derive(Debug)]
pub struct SkillPolicy {
    pub skills_enabled: bool,
    pub enabled: HashSet<String>,
    pub compatible_enabled: BTreeSet<String>,
}

impl SkillPolicy {
    pub fn effective(
        global: &AppConfig,
        role: Option<&Role>,
        agent: Option<&Agent>,
        session: Option<&Session>,
    ) -> Result<Self> {
        Self::effective_with(
            global,
            role,
            agent,
            session,
            &paths::has_skill,
            &paths::list_skills,
            &|name, mcp_on| {
                Skill::load(name)
                    .map(|s| s.is_compatible(mcp_on))
                    .unwrap_or(false)
            },
        )
    }

    fn effective_with<F, G, H>(
        global: &AppConfig,
        role: Option<&Role>,
        agent: Option<&Agent>,
        session: Option<&Session>,
        skill_exists: &F,
        list_installed: &G,
        skill_is_compatible: &H,
    ) -> Result<Self>
    where
        F: Fn(&str) -> bool,
        G: Fn() -> Vec<String>,
        H: Fn(&str, bool) -> bool,
    {
        let mut skills_enabled = global.skills_enabled;
        if let Some(r) = role
            && let Some(false) = r.skills_enabled()
        {
            skills_enabled = false;
        }

        if let Some(a) = agent
            && let Some(false) = a.skills_enabled()
        {
            skills_enabled = false;
        }

        if let Some(s) = session
            && let Some(false) = s.skills_enabled()
        {
            skills_enabled = false;
        }

        let visible: Option<HashSet<String>> = global
            .visible_skills
            .as_ref()
            .map(|v| v.iter().cloned().collect());

        let enabled_raw: Option<Vec<String>> = session
            .and_then(|s| s.enabled_skills().map(|v| v.to_vec()))
            .or_else(|| agent.and_then(|a| a.enabled_skills().map(|v| v.to_vec())))
            .or_else(|| role.and_then(|r| r.enabled_skills().map(|v| v.to_vec())))
            .or_else(|| global.enabled_skills.clone());

        let enabled: HashSet<String> = match enabled_raw {
            Some(explicit) => {
                let set: HashSet<String> = explicit.into_iter().collect();
                for name in &set {
                    paths::validate_skill_name(name).map_err(|e| {
                        anyhow!("enabled_skills contains invalid name '{name}': {e}")
                    })?;
                    match &visible {
                        Some(vs) => {
                            if !vs.contains(name) {
                                bail!(
                                    "enabled_skills references skill '{name}' which is not in the global 'visible_skills' allow-list"
                                );
                            }
                        }
                        None => {
                            if !skill_exists(name) {
                                bail!(
                                    "enabled_skills references skill '{name}' which is not installed"
                                );
                            }
                        }
                    }
                }
                set
            }
            None => match &visible {
                Some(v) => v.clone(),
                None => list_installed().into_iter().collect(),
            },
        };

        let compatible_enabled: BTreeSet<String> = if skills_enabled {
            let mcp_on = global.mcp_server_support;
            enabled
                .iter()
                .filter(|name| skill_is_compatible(name, mcp_on))
                .cloned()
                .collect()
        } else {
            BTreeSet::new()
        };

        Ok(Self {
            skills_enabled,
            enabled,
            compatible_enabled,
        })
    }

    pub fn allows(&self, name: &str) -> bool {
        self.skills_enabled && self.enabled.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::super::csv_to_vec;
    use super::*;

    fn always_true(_: &str) -> bool {
        true
    }

    fn empty_installed() -> Vec<String> {
        Vec::new()
    }

    fn all_compatible(_: &str, _: bool) -> bool {
        true
    }

    fn make_app_config(
        skills_enabled: bool,
        enabled: Option<&str>,
        visible: Option<&[&str]>,
    ) -> AppConfig {
        AppConfig {
            skills_enabled,
            enabled_skills: enabled.map(csv_to_vec),
            visible_skills: visible.map(|v| v.iter().map(|s| s.to_string()).collect()),
            ..AppConfig::default()
        }
    }

    #[test]
    fn defaults_yield_skills_enabled_with_empty_universe() {
        let global = AppConfig::default();

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(policy.skills_enabled);
        assert!(policy.enabled.is_empty());
    }

    #[test]
    fn falls_back_to_all_installed_when_no_level_sets_enabled_skills() {
        let global = AppConfig::default();
        let installed = || vec!["alpha".to_string(), "beta".to_string()];

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &installed,
            &all_compatible,
        )
        .unwrap();

        assert_eq!(policy.enabled.len(), 2);
        assert!(policy.enabled.contains("alpha"));
        assert!(policy.enabled.contains("beta"));
    }

    #[test]
    fn falls_back_to_visible_when_visible_set_but_no_enabled() {
        let global = make_app_config(true, None, Some(&["alpha", "beta"]));

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert_eq!(policy.enabled.len(), 2);
        assert!(policy.enabled.contains("alpha"));
        assert!(policy.enabled.contains("beta"));
    }

    #[test]
    fn global_enabled_skills_is_effective_when_no_other_levels() {
        let global = make_app_config(true, Some("alpha,beta"), Some(&["alpha", "beta", "gamma"]));

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(policy.enabled.contains("alpha"));
        assert!(policy.enabled.contains("beta"));
        assert!(!policy.enabled.contains("gamma"));
    }

    #[test]
    fn role_overrides_global_enabled_skills() {
        let global = make_app_config(true, Some("alpha"), Some(&["alpha", "beta"]));
        let role = Role::new("test", "---\nenabled_skills: beta\n---\nbody");

        let policy = SkillPolicy::effective_with(
            &global,
            Some(&role),
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(policy.enabled.contains("beta"));
        assert!(!policy.enabled.contains("alpha"));
    }

    #[test]
    fn any_skills_enabled_false_disables_globally() {
        let global = make_app_config(true, None, None);
        let role = Role::new("test", "---\nskills_enabled: false\n---\nbody");

        let policy = SkillPolicy::effective_with(
            &global,
            Some(&role),
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(!policy.skills_enabled);
    }

    #[test]
    fn allows_returns_false_when_skills_disabled() {
        let global = AppConfig {
            skills_enabled: false,
            ..AppConfig::default()
        };

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &|| vec!["alpha".to_string()],
            &all_compatible,
        )
        .unwrap();

        assert!(!policy.allows("alpha"));
    }

    #[test]
    fn allows_returns_true_when_skill_in_enabled_set() {
        let global = make_app_config(true, Some("alpha"), None);

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(policy.allows("alpha"));
        assert!(!policy.allows("beta"));
    }

    #[test]
    fn validation_rejects_uninstalled_skill_reference() {
        let global = make_app_config(true, Some("ghost"), None);

        let err = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &|_| false,
            &empty_installed,
            &all_compatible,
        )
        .unwrap_err();

        assert!(err.to_string().contains("not installed"));
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn validation_rejects_skill_not_in_visible_set() {
        let global = make_app_config(true, Some("beta"), Some(&["alpha"]));

        let err = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("not in the global 'visible_skills'")
        );
        assert!(err.to_string().contains("beta"));
    }

    #[test]
    fn validation_skipped_when_no_explicit_enabled_skills() {
        let global = make_app_config(true, None, None);

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &|_| false,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(policy.enabled.is_empty());
    }

    #[test]
    fn empty_string_enabled_skills_resolves_to_empty_override() {
        let global = make_app_config(true, Some("alpha,beta"), Some(&["alpha", "beta"]));
        let role = Role::new("test", "---\nenabled_skills: \"\"\n---\nbody");

        let policy = SkillPolicy::effective_with(
            &global,
            Some(&role),
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(policy.enabled.is_empty());
    }

    #[test]
    fn compatible_enabled_is_empty_when_skills_disabled() {
        let global = AppConfig {
            skills_enabled: false,
            enabled_skills: Some(vec!["alpha".into()]),
            visible_skills: Some(vec!["alpha".into()]),
            ..AppConfig::default()
        };

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert!(!policy.skills_enabled);
        assert!(policy.compatible_enabled.is_empty());
    }

    #[test]
    fn compatible_enabled_short_circuits_callback_when_skills_disabled() {
        use std::cell::Cell;
        let global = AppConfig {
            skills_enabled: false,
            enabled_skills: Some(vec!["alpha".into()]),
            visible_skills: Some(vec!["alpha".into()]),
            ..AppConfig::default()
        };
        let invoked = Cell::new(0u32);
        let counting = |_: &str, _: bool| {
            invoked.set(invoked.get() + 1);
            true
        };

        SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &counting,
        )
        .unwrap();

        assert_eq!(
            invoked.get(),
            0,
            "skill_is_compatible callback must not run when skills are disabled"
        );
    }

    #[test]
    fn compatible_enabled_includes_all_when_callback_passes() {
        let global = make_app_config(true, Some("alpha,beta"), Some(&["alpha", "beta"]));

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &all_compatible,
        )
        .unwrap();

        assert_eq!(policy.compatible_enabled.len(), 2);
        assert!(policy.compatible_enabled.contains("alpha"));
        assert!(policy.compatible_enabled.contains("beta"));
    }

    #[test]
    fn compatible_enabled_excludes_incompatible_skills() {
        let global = make_app_config(true, Some("alpha,beta"), Some(&["alpha", "beta"]));
        let only_alpha_compat = |name: &str, _: bool| name == "alpha";

        let policy = SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &only_alpha_compat,
        )
        .unwrap();

        assert!(policy.compatible_enabled.contains("alpha"));
        assert!(!policy.compatible_enabled.contains("beta"));
        assert_eq!(policy.compatible_enabled.len(), 1);
    }

    #[test]
    fn compatible_enabled_passes_mcp_flag_to_callback() {
        use std::cell::Cell;
        let global = AppConfig {
            skills_enabled: true,
            mcp_server_support: false,
            enabled_skills: Some(vec!["alpha".into()]),
            visible_skills: Some(vec!["alpha".into()]),
            ..AppConfig::default()
        };
        let observed_mcp = Cell::new(None::<bool>);
        let capture = |_: &str, mcp_on: bool| {
            observed_mcp.set(Some(mcp_on));
            true
        };

        SkillPolicy::effective_with(
            &global,
            None,
            None,
            None,
            &always_true,
            &empty_installed,
            &capture,
        )
        .unwrap();

        assert_eq!(
            observed_mcp.get(),
            Some(false),
            "callback must receive mcp_server_support flag from AppConfig"
        );
    }
}
