use super::agent::Agent;
use super::app_config::AppConfig;
use super::paths;
use super::role::Role;
use super::session::Session;

use anyhow::{Result, bail};
use std::collections::HashSet;

#[allow(dead_code)]
#[derive(Debug)]
pub struct SkillPolicy {
    pub skills_enabled: bool,
    pub visible: Option<HashSet<String>>,
    pub enabled: HashSet<String>,
}

#[allow(dead_code)]
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
        )
    }

    fn effective_with<F, G>(
        global: &AppConfig,
        role: Option<&Role>,
        agent: Option<&Agent>,
        session: Option<&Session>,
        skill_exists: &F,
        list_installed: &G,
    ) -> Result<Self>
    where
        F: Fn(&str) -> bool,
        G: Fn() -> Vec<String>,
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
            .and_then(|s| parse_csv_opt(s.enabled_skills()))
            .or_else(|| agent.and_then(|a| a.enabled_skills().map(|v| v.to_vec())))
            .or_else(|| role.and_then(|r| parse_csv_opt(r.enabled_skills())))
            .or_else(|| parse_csv_opt(global.enabled_skills.as_deref()));

        let enabled: HashSet<String> = match enabled_raw {
            Some(explicit) => {
                let set: HashSet<String> = explicit.into_iter().collect();
                for name in &set {
                    if !skill_exists(name) {
                        bail!("enabled_skills references skill '{name}' which is not installed");
                    }

                    if let Some(vs) = &visible
                        && !vs.contains(name)
                    {
                        bail!(
                            "enabled_skills references skill '{name}' which is not in visible_skills"
                        );
                    }
                }
                set
            }
            None => match &visible {
                Some(v) => v.clone(),
                None => list_installed().into_iter().collect(),
            },
        };

        Ok(Self {
            skills_enabled,
            visible,
            enabled,
        })
    }

    pub fn allows(&self, name: &str) -> bool {
        self.skills_enabled && self.enabled.contains(name)
    }
}

fn parse_csv_opt(s: Option<&str>) -> Option<Vec<String>> {
    s.map(|raw| {
        raw.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_true(_: &str) -> bool {
        true
    }

    fn empty_installed() -> Vec<String> {
        Vec::new()
    }

    fn make_app_config(
        skills_enabled: bool,
        enabled: Option<&str>,
        visible: Option<&[&str]>,
    ) -> AppConfig {
        AppConfig {
            skills_enabled,
            enabled_skills: enabled.map(|s| s.to_string()),
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
        )
        .unwrap();

        assert!(policy.skills_enabled);
        assert!(policy.visible.is_none());
        assert!(policy.enabled.is_empty());
    }

    #[test]
    fn falls_back_to_all_installed_when_no_level_sets_enabled_skills() {
        let global = AppConfig::default();
        let installed = || vec!["alpha".to_string(), "beta".to_string()];

        let policy =
            SkillPolicy::effective_with(&global, None, None, None, &always_true, &installed)
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
        )
        .unwrap();

        assert!(policy.enabled.contains("alpha"));
        assert!(policy.enabled.contains("beta"));
        assert!(!policy.enabled.contains("gamma"));
    }

    #[test]
    fn role_overrides_global_enabled_skills() {
        let global = make_app_config(true, Some("alpha"), Some(&["alpha", "beta"]));
        let role = Role::new(
            "test",
            "---\nenabled_skills: beta\n---\nbody",
        );

        let policy = SkillPolicy::effective_with(
            &global,
            Some(&role),
            None,
            None,
            &always_true,
            &empty_installed,
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
        )
        .unwrap_err();

        assert!(err.to_string().contains("not in visible_skills"));
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
        )
        .unwrap();

        assert!(policy.enabled.is_empty());
    }
}
