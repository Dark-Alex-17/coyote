//! Test-only helpers shared across modules' unit tests.

use crate::config::{WORKSPACE_COYOTE_DIR_NAME, memory};
use crate::utils;
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

struct TestLogCollector;

static WARN_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static DEBUG_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static INFO_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// Debug and info capture are restricted to the modules whose tests assert
/// on that output so the buffers are not flooded by the rest of the crate.
/// Matching is module-boundary-aware: only these exact modules or their `::`
/// submodules qualify, so a sibling like `hooks_registry` would not.
const DEBUG_TARGET_PREFIXES: [&str; 4] = [
    concat!(env!("CARGO_CRATE_NAME"), "::hooks"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::agent"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::child_sessions"),
    concat!(env!("CARGO_CRATE_NAME"), "::function"),
];

const INFO_TARGET_PREFIXES: [&str; 3] = [
    concat!(env!("CARGO_CRATE_NAME"), "::graph::script"),
    concat!(env!("CARGO_CRATE_NAME"), "::function"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::child_sessions"),
];

fn captures_warn(metadata: &Metadata) -> bool {
    metadata.level() <= Level::Warn
}

fn captures_module_level(metadata: &Metadata, level: Level, prefixes: &[&str]) -> bool {
    if metadata.level() != level {
        return false;
    }
    let target = metadata.target();
    prefixes.iter().any(|prefix| {
        target == *prefix
            || target
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

fn captures_module_debug(metadata: &Metadata) -> bool {
    captures_module_level(metadata, Level::Debug, &DEBUG_TARGET_PREFIXES)
}

fn captures_module_info(metadata: &Metadata) -> bool {
    captures_module_level(metadata, Level::Info, &INFO_TARGET_PREFIXES)
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

/// Every info-level message from the [`INFO_TARGET_PREFIXES`] modules
/// captured since the collector was installed. Same marker-filtering
/// discipline as [`warn_messages`].
pub(crate) fn info_messages() -> &'static Mutex<Vec<String>> {
    INFO_MESSAGES.get_or_init(Mutex::default)
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

/// A copy of everything in [`info_messages`], recovering from poisoning the
/// same way the collector itself does.
pub(crate) fn info_snapshot() -> Vec<String> {
    snapshot_of(info_messages())
}

fn snapshot_of(buffer: &Mutex<Vec<String>>) -> Vec<String> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

impl Log for TestLogCollector {
    fn enabled(&self, metadata: &Metadata) -> bool {
        captures_warn(metadata) || captures_module_debug(metadata) || captures_module_info(metadata)
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
        } else if captures_module_info(record.metadata()) {
            info_messages()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// Installs the process-wide log collector capturing warn-level messages and
/// debug/info messages from the [`DEBUG_TARGET_PREFIXES`] and
/// [`INFO_TARGET_PREFIXES`] modules. `log::set_logger` accepts one logger per
/// process, so every test that captures any stream must install through this
/// shared entry point. The max level is Debug so the debug capture sees its
/// records; warn capture is unaffected.
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
/// land under the temp config dir rather than the developer's real one, and
/// the cache dir is pinned to a fresh temp dir so nothing a test spawns can
/// write into the developer's real cache.
/// Workspace memory discovery starts at the temp workspace root (which has
/// no `memory/MEMORY.md`), so the repo's own memory index never reaches a
/// test prompt.
/// Tests using it must serialize (`#[serial]`) — the env vars are
/// process-global.
pub(crate) struct TestConfigDirGuard {
    _env: EnvVarGuard,
    _workspace_env: EnvVarGuard,
    _sessions_env: EnvVarGuard,
    _workspace_memory_root_env: EnvVarGuard,
    pub(crate) path: PathBuf,
    workspace_path: PathBuf,
    cache_root: PathBuf,
    _cache_env: EnvVarGuard,
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
        let cache_root = std::env::temp_dir().join(format!("coyote-{label}-cache-{unique}"));
        std::fs::create_dir_all(&cache_root).unwrap();
        let workspace_root = workspace_path
            .parent()
            .expect("workspace config dir sits inside its workspace root");
        Self {
            _env: EnvVarGuard::set(utils::get_env_name("config_dir"), &path),
            _workspace_env: EnvVarGuard::set(
                utils::get_env_name("workspace_config_dir"),
                &workspace_path,
            ),
            _sessions_env: EnvVarGuard::unset(utils::get_env_name("sessions_dir")),
            _workspace_memory_root_env: EnvVarGuard::set(
                memory::WORKSPACE_MEMORY_ROOT_TEST_ENV,
                workspace_root,
            ),
            path,
            workspace_path,
            _cache_env: EnvVarGuard::set(utils::get_env_name("cache_dir"), &cache_root),
            cache_root,
        }
    }

    /// The temp dir `COYOTE_CACHE_DIR` points at for the guard's lifetime.
    pub(crate) fn cache_root(&self) -> &Path {
        &self.cache_root
    }
}

impl Drop for TestConfigDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        if let Some(root) = self.workspace_path.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
        let _ = std::fs::remove_dir_all(&self.cache_root);
    }
}

/// Points the cache dir at a fresh temp directory for the guard's lifetime
/// and removes it on drop, so the developer's real sbx kit cache is never
/// read or overwritten by a test. Tests using it must serialize
/// (`#[serial]`); the env var is process-global.
pub(crate) struct CacheDirGuard {
    pub(crate) root: PathBuf,
    _env: EnvVarGuard,
}

impl CacheDirGuard {
    pub(crate) fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("coyote-cache-{label}-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        let env = EnvVarGuard::set(utils::get_env_name("cache_dir"), &root);
        Self { root, _env: env }
    }
}

impl Drop for CacheDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Puts the mode back on a path when dropped, so a failing assertion
/// cannot leave a directory the next run cannot clean up.
#[cfg(unix)]
pub(crate) struct RestoreMode(pub(crate) PathBuf, pub(crate) u32);

#[cfg(unix)]
impl Drop for RestoreMode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
    }
}

/// True when a write inside `dir` succeeds despite its mode bits, i.e.
/// the process runs with privileges (root, CAP_DAC_OVERRIDE) that make a
/// read-only directory unstageable; callers skip the scenario. The probe
/// file is removed again.
#[cfg(unix)]
pub(crate) fn mode_bits_are_ignored_in(dir: &Path) -> bool {
    let probe = dir.join("probe");
    if std::fs::write(&probe, "").is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&probe);
    true
}

/// The first bash on PATH outside System32, for tests that drive the shipped
/// shell scripts. The one Windows puts in System32 is the WSL launcher, not
/// a shell: it prints a banner about missing distributions and exits 1, and
/// `Command::new("bash")` by bare name finds it ahead of Git Bash, so tests
/// must spawn the path this returns.
pub(crate) fn bash_program() -> Option<PathBuf> {
    which::which_all("bash")
        .ok()?
        .find(|path| is_outside_system32(path))
}

/// Twin of `is_outside_system32` in tests/agent_gate_scripts.rs; change
/// both together. Splits on both separators because `Path::components`
/// leaves a Windows path whole on unix, where this is unit-tested.
fn is_outside_system32(path: &Path) -> bool {
    !path
        .to_string_lossy()
        .split(['/', '\\'])
        .any(|part| part.eq_ignore_ascii_case("system32"))
}

mod tests {
    use super::{TestConfigDirGuard, is_outside_system32};
    use crate::utils;
    use serial_test::serial;
    use std::path::Path;

    #[test]
    fn is_outside_system32_reads_windows_and_unix_paths() {
        for launcher in [
            r"C:\Windows\System32\bash.exe",
            r"c:\windows\SYSTEM32\bash.exe",
            "C:/Windows/System32/bash.exe",
            r"\\?\C:\Windows\System32\bash.exe",
        ] {
            assert!(!is_outside_system32(Path::new(launcher)), "{launcher}");
        }
        for shell in [r"C:\Program Files\Git\bin\bash.exe", "/usr/bin/bash"] {
            assert!(is_outside_system32(Path::new(shell)), "{shell}");
        }
    }

    #[test]
    #[serial]
    fn config_dir_guard_pins_the_cache_dir_and_restores_it_on_drop() {
        let key = utils::get_env_name("cache_dir");
        let before = std::env::var_os(&key);
        let cache_root = {
            let guard = TestConfigDirGuard::new("testing-cache-pin");
            let pinned = std::env::var_os(&key).expect("cache dir pinned while the guard lives");
            assert_eq!(Path::new(&pinned), guard.cache_root());
            assert!(guard.cache_root().is_dir());
            assert_ne!(
                guard.cache_root(),
                guard.path,
                "the cache dir is not the config dir"
            );
            guard.cache_root().to_path_buf()
        };
        assert_eq!(std::env::var_os(&key), before);
        assert!(
            !cache_root.exists(),
            "the temp cache dir is removed on drop"
        );
    }
}
