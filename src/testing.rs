//! Test-only helpers shared across modules' unit tests.

use crate::config::WORKSPACE_COYOTE_DIR_NAME;
use crate::utils;
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::{Mutex, Once, OnceLock};

struct TestLogCollector;

static WARN_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static DEBUG_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// Debug capture is restricted to the modules whose tests assert on debug
/// output so the buffer is not flooded by the rest of the crate. Matching is
/// module-boundary-aware: only these exact modules or their `::` submodules
/// qualify, so a sibling like `hooks_registry` would not.
const DEBUG_TARGET_PREFIXES: [&str; 2] = [
    concat!(env!("CARGO_CRATE_NAME"), "::hooks"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::agent"),
];

fn captures_warn(metadata: &Metadata) -> bool {
    metadata.level() <= Level::Warn
}

fn captures_module_debug(metadata: &Metadata) -> bool {
    if metadata.level() != Level::Debug {
        return false;
    }
    let target = metadata.target();
    DEBUG_TARGET_PREFIXES.iter().any(|prefix| {
        target == *prefix
            || target
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// Every warn- or error-level message captured since the collector was
/// installed. The buffer is process-global and shared by all tests, so assert
/// by filtering for a marker unique to the test rather than on the whole
/// buffer.
pub(crate) fn warn_messages() -> &'static Mutex<Vec<String>> {
    WARN_MESSAGES.get_or_init(Mutex::default)
}

/// Every debug-level message from the [`DEBUG_TARGET_PREFIXES`] modules
/// captured since the collector was installed. Same marker-filtering
/// discipline as [`warn_messages`].
pub(crate) fn debug_messages() -> &'static Mutex<Vec<String>> {
    DEBUG_MESSAGES.get_or_init(Mutex::default)
}

/// A copy of everything in [`warn_messages`], recovering from poisoning the
/// same way the collector itself does.
pub(crate) fn warn_snapshot() -> Vec<String> {
    snapshot_of(warn_messages())
}

/// A copy of everything in [`debug_messages`], recovering from poisoning the
/// same way the collector itself does.
pub(crate) fn debug_snapshot() -> Vec<String> {
    snapshot_of(debug_messages())
}

fn snapshot_of(buffer: &Mutex<Vec<String>>) -> Vec<String> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

impl Log for TestLogCollector {
    fn enabled(&self, metadata: &Metadata) -> bool {
        captures_warn(metadata) || captures_module_debug(metadata)
    }

    // A test that panics while holding a buffer poisons its mutex; the logger
    // recovers via `into_inner` so every later test can still log.
    fn log(&self, record: &Record) {
        if captures_warn(record.metadata()) {
            warn_messages()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(record.args().to_string());
        } else if captures_module_debug(record.metadata()) {
            debug_messages()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// Installs the process-wide log collector capturing warn-level messages and
/// debug messages from the [`DEBUG_TARGET_PREFIXES`] modules.
/// `log::set_logger` accepts one logger per process, so every test that
/// captures either stream must install through this shared entry point. The
/// max level is Debug so the debug capture sees its records; warn capture is
/// unaffected.
pub(crate) fn install_log_collector() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&TestLogCollector).expect("no other logger should be installed");
        log::set_max_level(LevelFilter::Debug);
    });
}

/// Sets (or removes) an environment variable for the guard's lifetime and
/// restores the previous value on drop, including on panic, so a failing
/// assertion cannot leak the override into subsequent tests. Tests using it
/// must serialize (`#[serial]`) — the process environment is global.
pub(crate) struct EnvVarGuard {
    key: String,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: impl Into<String>, value: impl AsRef<OsStr>) -> Self {
        let key = key.into();
        let previous = std::env::var_os(&key);
        unsafe {
            std::env::set_var(&key, value);
        }
        Self { key, previous }
    }

    pub(crate) fn unset(key: impl Into<String>) -> Self {
        let key = key.into();
        let previous = std::env::var_os(&key);
        unsafe {
            std::env::remove_var(&key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(previous) => std::env::set_var(&self.key, previous),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

/// Points the config dir and the workspace config dir at fresh temp
/// directories for the guard's lifetime and removes them on drop, including
/// on panic, so the developer's real `./.coyote/` never leaks into a test.
/// The sessions-dir override is cleared too, so Global-scope session writes
/// land under the temp config dir rather than the developer's real one.
/// Tests using it must serialize (`#[serial]`) — the env vars are
/// process-global.
pub(crate) struct TestConfigDirGuard {
    _env: EnvVarGuard,
    _workspace_env: EnvVarGuard,
    _sessions_env: EnvVarGuard,
    pub(crate) path: PathBuf,
    workspace_path: PathBuf,
}

impl TestConfigDirGuard {
    pub(crate) fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("coyote-{label}-{unique}"));
        std::fs::create_dir_all(&path).unwrap();
        let workspace_path = std::env::temp_dir()
            .join(format!("coyote-{label}-ws-{unique}"))
            .join(WORKSPACE_COYOTE_DIR_NAME);
        std::fs::create_dir_all(&workspace_path).unwrap();
        Self {
            _env: EnvVarGuard::set(utils::get_env_name("config_dir"), &path),
            _workspace_env: EnvVarGuard::set(
                utils::get_env_name("workspace_config_dir"),
                &workspace_path,
            ),
            _sessions_env: EnvVarGuard::unset(utils::get_env_name("sessions_dir")),
            path,
            workspace_path,
        }
    }
}

impl Drop for TestConfigDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        if let Some(root) = self.workspace_path.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
