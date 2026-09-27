//! Test-only helpers shared across modules' unit tests.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, Once, OnceLock};

struct TestLogCollector;

static WARN_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static DEBUG_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// Debug capture is restricted to the modules whose tests assert on debug
/// output so the buffer is not flooded by the rest of the crate. Matching is
/// module-boundary-aware: only these exact modules or their `::` submodules
/// qualify, so a sibling like `hooks_registry` would not.
const DEBUG_TARGET_PREFIXES: [&str; 3] = [
    concat!(env!("CARGO_CRATE_NAME"), "::hooks"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::agent"),
    concat!(env!("CARGO_CRATE_NAME"), "::mesh"),
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

/// Points the config dir at a fresh temp directory for the guard's lifetime
/// and removes it on drop, including on panic. Tests using it must serialize
/// (`#[serial]`) — the config-dir env var is process-global.
pub(crate) struct TestConfigDirGuard {
    _env: EnvVarGuard,
    pub(crate) path: std::path::PathBuf,
}

impl TestConfigDirGuard {
    pub(crate) fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("coyote-{label}-{unique}"));
        std::fs::create_dir_all(&path).unwrap();
        Self {
            _env: EnvVarGuard::set(crate::utils::get_env_name("config_dir"), &path),
            path,
        }
    }
}

impl Drop for TestConfigDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Whether a process with `pid` still exists. A pid that exists but cannot
/// be signalled or queried counts as alive.
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs only the existence and permission checks.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let queried = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        queried && code == STILL_ACTIVE as u32
    }
}
