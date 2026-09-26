use arc_swap::ArcSwapOption;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

pub const ENVOY_AGENT_NAME: &str = "envoy";

pub const RESERVED_AGENT_NAMES: [&str; 1] = [ENVOY_AGENT_NAME];

pub const ENVOY_BUILTIN_DESCRIPTION: &str = "The mesh envoy that answers peers on the user's \
    behalf; running it directly shows what it would say.";

/// ASCII-lowercases `name` and strips every `-` and `_`, so spellings that
/// differ only in case or separators (`Envoy`, `en-voy`, `en_voy`) compare
/// equal to the reserved name. Filesystem aliases collapse too: Windows
/// ignores trailing dots and spaces (`envoy.`, `envoy `) and treats `:` as
/// an alternate-data-stream separator (`envoy:stream`), so those must not
/// slip past the reservation either, and neither must path spellings that
/// resolve to the same directory (`./envoy`, `envoy/`, `envoy/.`).
pub fn normalize_agent_name(name: &str) -> String {
    let name = name
        .strip_prefix("./")
        .or_else(|| name.strip_prefix(".\\"))
        .unwrap_or(name);
    let name = name
        .strip_suffix("/.")
        .or_else(|| name.strip_suffix("\\."))
        .unwrap_or(name)
        .trim_end_matches(['/', '\\']);
    let name = name.split_once(':').map_or(name, |(before, _)| before);
    name.trim_end_matches(['.', ' '])
        .chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Resolves any accepted spelling of a reserved name (`Envoy`, `en-voy`) to
/// its canonical `RESERVED_AGENT_NAMES` entry. Everything downstream of the
/// reservation check (data-dir wiring, messages, dedupe keys) works on the
/// canonical name so the caller's spelling never leaks into a path.
pub fn reserved_agent(name: &str) -> Option<&'static str> {
    let normalized = normalize_agent_name(name);
    RESERVED_AGENT_NAMES
        .iter()
        .copied()
        .find(|reserved| normalize_agent_name(reserved) == normalized)
}

/// Description shown for a built-in while no source is registered.
pub fn builtin_default_description(name: &str) -> &'static str {
    let Some(canonical) = reserved_agent(name) else {
        return "";
    };
    match canonical {
        ENVOY_AGENT_NAME => ENVOY_BUILTIN_DESCRIPTION,
        _ => "",
    }
}

/// Refusal for callers that try to run a reserved agent themselves. `tail`
/// names what was attempted (`it cannot be spawned`, ...).
pub fn reserved_agent_refusal(requested: &str, canonical: &str, tail: &str) -> String {
    format!(
        "Agent '{requested}' is reserved: only a human can run it (`.agent {canonical}`); {tail}"
    )
}

/// Resolves a reserved agent name to its built-in definition. Registered by
/// whichever subsystem materializes the built-in (the mesh, for the envoy);
/// while nothing is registered the agent is unavailable.
///
/// Both methods always receive the CANONICAL name from `RESERVED_AGENT_NAMES`.
/// The registry is the path seam: `paths::agent_data_dir(name)`,
/// `paths::agent_config_file(name)` and everything derived from them resolve
/// a reserved name through the registered source, and `<NAME>_DATA_DIR` /
/// `<NAME>_CONFIG_FILE` are ignored for reserved names. `Agent::init`
/// refuses a returned dir that lives inside `paths::agents_data_dir()`.
pub trait BuiltinAgentSource: Send + Sync {
    fn agent_dir(&self, name: &str) -> Option<PathBuf>;
    fn description(&self, name: &str) -> Option<String>;
}

static BUILTIN_AGENT_SOURCE: ArcSwapOption<Arc<dyn BuiltinAgentSource>> =
    ArcSwapOption::const_empty();

// Called by the mesh once it materializes the envoy.
#[cfg_attr(not(test), expect(dead_code))]
pub fn register_builtin_source(source: Arc<dyn BuiltinAgentSource>) {
    BUILTIN_AGENT_SOURCE.store(Some(Arc::new(source)));
}

pub fn builtin_agent_dir(name: &str) -> Option<PathBuf> {
    let canonical = reserved_agent(name)?;
    BUILTIN_AGENT_SOURCE
        .load()
        .as_ref()
        .and_then(|source| source.agent_dir(canonical))
}

pub fn builtin_agent_description(name: &str) -> Option<String> {
    let canonical = reserved_agent(name)?;
    BUILTIN_AGENT_SOURCE
        .load()
        .as_ref()
        .and_then(|source| source.description(canonical))
}

/// Registers a source for the guard's lifetime and clears the registry on
/// drop, including on panic. The registry is process-global, so tests using
/// it must serialize (`#[serial]`).
#[cfg(test)]
pub(crate) struct BuiltinSourceGuard;

#[cfg(test)]
impl BuiltinSourceGuard {
    pub(crate) fn new(source: Arc<dyn BuiltinAgentSource>) -> Self {
        register_builtin_source(source);
        Self
    }
}

#[cfg(test)]
impl Drop for BuiltinSourceGuard {
    fn drop(&mut self) {
        BUILTIN_AGENT_SOURCE.store(None);
    }
}

/// Test source that serves every reserved name from one directory and
/// leaves the description to `builtin_default_description`.
#[cfg(test)]
pub(crate) struct FixedDirSource(pub(crate) PathBuf);

#[cfg(test)]
impl BuiltinAgentSource for FixedDirSource {
    fn agent_dir(&self, _name: &str) -> Option<PathBuf> {
        Some(self.0.clone())
    }

    fn description(&self, _name: &str) -> Option<String> {
        None
    }
}

/// A reserved agent was requested before its built-in definition was
/// registered. Callers can `downcast_ref` an `anyhow::Error` to this type to
/// distinguish it from a missing user agent.
#[derive(Debug)]
pub struct BuiltinAgentUnavailable {
    pub name: String,
}

impl fmt::Display for BuiltinAgentUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Agent '{}' is built in and is not available yet (enable `mesh:` to materialize it)",
            self.name
        )
    }
}

impl std::error::Error for BuiltinAgentUnavailable {}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::Mutex;

    struct RecordingSource {
        dir: PathBuf,
        seen: Mutex<Vec<String>>,
    }

    impl BuiltinAgentSource for RecordingSource {
        fn agent_dir(&self, name: &str) -> Option<PathBuf> {
            self.seen.lock().unwrap().push(name.to_string());
            Some(self.dir.clone())
        }

        fn description(&self, name: &str) -> Option<String> {
            self.seen.lock().unwrap().push(name.to_string());
            Some("from source".to_string())
        }
    }

    #[test]
    fn reserved_predicate_ignores_case_and_separators() {
        for name in ["envoy", "Envoy", "ENVOY", "en-voy", "en_voy", "E-n_VOY"] {
            assert!(reserved_agent(name).is_some(), "{name} must be reserved");
        }
    }

    #[test]
    fn reserved_predicate_collapses_filesystem_aliases() {
        for name in [
            "envoy.",
            "envoy ",
            "envoy..  ",
            "envoy:stream",
            "Envoy.:$DATA",
            "envoy/",
            "envoy/.",
            "./envoy",
            "envoy\\",
        ] {
            assert_eq!(reserved_agent(name), Some("envoy"), "{name:?}");
        }
    }

    #[test]
    fn reserved_predicate_rejects_near_misses() {
        for name in [
            "envoys",
            "envo",
            "",
            "rag",
            "envoy2",
            "the-envoy",
            ".envoy",
            " envoy",
            ":envoy",
        ] {
            assert!(
                reserved_agent(name).is_none(),
                "{name:?} must not be reserved"
            );
        }
    }

    #[test]
    fn reserved_agent_resolves_to_canonical_name() {
        assert_eq!(reserved_agent("En-Voy"), Some("envoy"));
        assert_eq!(reserved_agent("envoy"), Some(ENVOY_AGENT_NAME));
        assert_eq!(reserved_agent("envoys"), None);
    }

    #[test]
    fn builtin_default_description_is_empty_for_unknown_names() {
        assert_eq!(
            builtin_default_description(ENVOY_AGENT_NAME),
            ENVOY_BUILTIN_DESCRIPTION
        );
        assert_eq!(builtin_default_description("rag"), "");
    }

    #[test]
    fn builtin_default_description_canonicalises_spelling() {
        assert_eq!(
            builtin_default_description("Envoy"),
            ENVOY_BUILTIN_DESCRIPTION
        );
        assert_eq!(
            builtin_default_description("en-voy"),
            ENVOY_BUILTIN_DESCRIPTION
        );
    }

    #[test]
    fn refusal_names_requested_and_canonical_spellings() {
        assert_eq!(
            reserved_agent_refusal("En-voy", "envoy", "it cannot be spawned"),
            "Agent 'En-voy' is reserved: only a human can run it (`.agent envoy`); it cannot be spawned"
        );
    }

    #[test]
    fn normalize_strips_separators_and_lowercases() {
        assert_eq!(normalize_agent_name("En-Voy_X"), "envoyx");
    }

    #[test]
    fn reserved_names_are_canonical_and_described() {
        for name in RESERVED_AGENT_NAMES {
            assert_eq!(normalize_agent_name(name), name, "{name} must be canonical");
            assert!(
                !builtin_default_description(name).is_empty(),
                "{name} needs a default description"
            );
        }
    }

    #[test]
    #[serial]
    fn registry_is_empty_by_default() {
        assert_eq!(builtin_agent_dir(ENVOY_AGENT_NAME), None);
        assert_eq!(builtin_agent_description(ENVOY_AGENT_NAME), None);
    }

    #[test]
    #[serial]
    fn guard_registers_then_clears_and_passes_canonical_names() {
        let dir = PathBuf::from("/tmp/envoy-builtin");
        let source = Arc::new(RecordingSource {
            dir: dir.clone(),
            seen: Mutex::new(Vec::new()),
        });
        {
            let _guard = BuiltinSourceGuard::new(source.clone());
            assert_eq!(builtin_agent_dir("Envoy"), Some(dir.clone()));
            assert_eq!(
                builtin_agent_description("en-voy").as_deref(),
                Some("from source")
            );
            assert_eq!(builtin_agent_dir("rag"), None);
            assert_eq!(builtin_agent_description("rag"), None);
        }
        assert_eq!(
            *source.seen.lock().unwrap(),
            vec!["envoy".to_string(), "envoy".to_string()],
            "non-reserved names never reach the source; reserved ones arrive canonical"
        );
        assert_eq!(builtin_agent_dir(ENVOY_AGENT_NAME), None);
        assert_eq!(builtin_agent_description(ENVOY_AGENT_NAME), None);
    }

    #[test]
    fn unavailable_error_downcasts_through_anyhow() {
        let err: anyhow::Error = BuiltinAgentUnavailable {
            name: "envoy".to_string(),
        }
        .into();
        let typed = err.downcast_ref::<BuiltinAgentUnavailable>().unwrap();
        assert_eq!(typed.name, "envoy");
        assert_eq!(
            err.to_string(),
            "Agent 'envoy' is built in and is not available yet (enable `mesh:` to materialize it)"
        );
    }
}
