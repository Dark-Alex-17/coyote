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
/// `config::request_context` is in for the completer's knock-cache read pin.
/// Matching is module-boundary-aware: only these exact modules or their `::`
/// submodules qualify, so a sibling like `hooks_registry` would not.
const DEBUG_TARGET_PREFIXES: [&str; 6] = [
    concat!(env!("CARGO_CRATE_NAME"), "::hooks"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::agent"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::request_context"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::child_sessions"),
    concat!(env!("CARGO_CRATE_NAME"), "::function"),
    concat!(env!("CARGO_CRATE_NAME"), "::mesh"),
];

/// Info capture also covers the mesh node start and the `inbox_dir` resolver,
/// whose tests assert that a translation is announced at info level exactly
/// once and never by the resolver itself.
const INFO_TARGET_PREFIXES: [&str; 5] = [
    concat!(env!("CARGO_CRATE_NAME"), "::graph::script"),
    concat!(env!("CARGO_CRATE_NAME"), "::function"),
    concat!(env!("CARGO_CRATE_NAME"), "::mesh::node"),
    concat!(env!("CARGO_CRATE_NAME"), "::config::mesh_config"),
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

/// Clears the process umask so the modes a test reads back are the ones the code
/// asked for, and restores the previous umask on drop, including on panic. The umask
/// is process-global, so tests using it must serialize (`#[serial]`).
#[cfg(unix)]
pub(crate) struct UmaskGuard(libc::mode_t);

#[cfg(unix)]
impl UmaskGuard {
    pub(crate) fn zero() -> Self {
        Self(unsafe { libc::umask(0) })
    }
}

#[cfg(unix)]
impl Drop for UmaskGuard {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.0);
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
/// no `memory/MEMORY.md`) and never walks above it, so the repo's own memory
/// index never reaches a test prompt.
/// Tests using it must serialize (`#[serial]`) — the env vars are
/// process-global.
pub(crate) struct TestConfigDirGuard {
    _env: EnvVarGuard,
    _workspace_env: EnvVarGuard,
    _sessions_env: EnvVarGuard,
    _workspace_memory_root_env: EnvVarGuard,
    pub(crate) path: PathBuf,
    workspace_root: PathBuf,
    cache_root: PathBuf,
    _cache_env: EnvVarGuard,
}

impl TestConfigDirGuard {
    pub(crate) fn new(label: &str) -> Self {
        Self::new_under(label, &std::env::temp_dir())
    }

    /// `new` with the temp dirs created inside `base`, so a test can plant
    /// files above the pinned workspace root without touching the shared
    /// system temp dir.
    pub(crate) fn new_under(label: &str, base: &Path) -> Self {
        assert!(
            base.is_absolute(),
            "the guard's base dir must be absolute, got {}",
            base.display()
        );
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = base.join(format!("coyote-{label}-{unique}"));
        std::fs::create_dir_all(&path).unwrap();
        let workspace_root = base.join(format!("coyote-{label}-ws-{unique}"));
        let workspace_path = workspace_root.join(WORKSPACE_COYOTE_DIR_NAME);
        std::fs::create_dir_all(&workspace_path).unwrap();
        let cache_root = base.join(format!("coyote-{label}-cache-{unique}"));
        std::fs::create_dir_all(&cache_root).unwrap();
        Self {
            _env: EnvVarGuard::set(utils::get_env_name("config_dir"), &path),
            _workspace_env: EnvVarGuard::set(
                utils::get_env_name("workspace_config_dir"),
                &workspace_path,
            ),
            _sessions_env: EnvVarGuard::unset(utils::get_env_name("sessions_dir")),
            _workspace_memory_root_env: EnvVarGuard::set(
                memory::WORKSPACE_MEMORY_ROOT_TEST_ENV,
                &workspace_root,
            ),
            path,
            workspace_root,
            _cache_env: EnvVarGuard::set(utils::get_env_name("cache_dir"), &cache_root),
            cache_root,
        }
    }

    /// The parent of the pinned workspace config dir, and the root workspace
    /// memory discovery is pinned to.
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// The temp dir `COYOTE_CACHE_DIR` points at for the guard's lifetime.
    pub(crate) fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// The name of the config dir env var this guard pins, for tests that
    /// check a rendered script never mentions it.
    pub(crate) fn config_dir_env_name() -> String {
        utils::get_env_name("config_dir")
    }

    /// Unsets the config dir env var until the returned guard drops, which
    /// puts the pinned dir back, so a test can exercise the fallback for an
    /// unset config dir while every other dir stays pinned.
    pub(crate) fn unset_config_dir_env(&self) -> EnvVarGuard {
        EnvVarGuard::unset(Self::config_dir_env_name())
    }
}

impl Drop for TestConfigDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.workspace_root);
        let _ = std::fs::remove_dir_all(&self.cache_root);
    }
}

/// Whether a process with `pid` still exists. A pid that exists but cannot
/// be signalled or queried counts as alive.
///
/// The unix arm asks the kernel directly through `libc::kill(pid, 0)` rather than
/// reusing `config::envoy::pid_alive`: that one shells out to `ps` where `/proc` is
/// absent, and a test asserting that a timeout kill reached its interpreter must not
/// depend on an external tool being installed.
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs only the existence and permission checks.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Windows arm of [`pid_alive`]: `OpenProcess` + `GetExitCodeProcess`.
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
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};

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
            assert_ne!(
                guard.cache_root(),
                guard.workspace_root(),
                "the cache dir is not the workspace root"
            );
            guard.cache_root().to_path_buf()
        };
        assert_eq!(std::env::var_os(&key), before);
        assert!(
            !cache_root.exists(),
            "the temp cache dir is removed on drop"
        );
    }

    #[test]
    fn no_source_file_bypasses_the_workspace_memory_pin() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let sources: Vec<(PathBuf, String)> = rust_sources(&src)
            .into_iter()
            .map(|path| {
                let source = fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                (path.strip_prefix(&src).unwrap().to_path_buf(), source)
            })
            .collect();
        let sources: Vec<(&Path, &str)> = sources
            .iter()
            .map(|(path, source)| (path.as_path(), source.as_str()))
            .collect();
        let offences = offences_in(&sources);
        assert!(offences.is_empty(), "{}", offences.join("\n"));
    }

    /// Workspace memory lookups go through the pinned entry point in
    /// src/config/memory.rs, git-root walks outside it are bounded, and tests
    /// pin the config dir only through the shared guard, so no test can reach
    /// the developer's real workspace memory or config dir through a raw
    /// ancestor walk or a private guard that skips the pins. `sources` are
    /// `(path relative to src/, source)` pairs. Each line is judged by the
    /// scope it sits in (a `#[cfg(test)]` item, a file declared as a
    /// `#[cfg(test)]` module or carrying `#![cfg(test)]`, the innermost
    /// `fn`), not by its file; a `#[path]` module is refused because its file
    /// would escape that resolution. Walker and `--init-memory` helper names
    /// are matched as identifier tokens on the line with literals and
    /// comments blanked, so a point-free use or an aliased import counts and
    /// a mention in a string or comment does not. The git root walkers are
    /// allowed only in the fns of `GIT_ROOT_ALLOW`: the walk's own
    /// definitions (`find_git_root`, `find_git_root_within`,
    /// `find_workspace_git_root`, `find_git_root_under`) plus the two
    /// `--init-memory` helpers. memory.rs tests get no blanket exemption for
    /// them and use `find_git_root_under` with their own temp root as the
    /// ceiling. The raw memory walk is allowed in the fns of `RAW_WALK_HOMES`,
    /// and in memory.rs's `#[cfg(test)]` code only as a one-line call whose
    /// ceiling is `Some(..)`. The helpers are allowed in their own
    /// definitions, memory.rs's `#[cfg(test)]` code and src/main.rs. The
    /// guard struct and config dir needles match raw text and are split so
    /// this file's own source does not match them.
    fn offences_in(sources: &[(&Path, &str)]) -> Vec<String> {
        const RAW_WALK: &str = "walk_workspace_memory";
        const RAW_WALK_HOMES: [&str; 2] = [RAW_WALK, "find_workspace_memory"];
        const GUARD_DEFINITION: &str = concat!("struct TestConfig", "DirGuard");
        const CONFIG_DIR_ENV: &str = concat!("env_name(\"config", "_dir\")");
        const GIT_ROOT_WALKS: [&str; 2] = ["find_git_root", "find_git_root_within"];
        // Unbounded on purpose: they serve the explicit `--init-memory`
        // command in src/main.rs.
        const INIT_MEMORY_HELPERS: [&str; 2] =
            ["init_workspace_root", "write_workspace_memory_index"];
        // The walker's own definitions and the explicit `--init-memory`
        // command, which roots the store at the real git root on purpose.
        const GIT_ROOT_ALLOW: [(&str, &str); 6] = [
            ("config/memory.rs", "find_git_root"),
            ("config/memory.rs", "find_git_root_within"),
            ("config/memory.rs", "find_workspace_git_root"),
            ("config/memory.rs", "find_git_root_under"),
            ("config/memory.rs", "init_workspace_root"),
            ("config/memory.rs", "write_workspace_memory_index"),
        ];
        // Tests that check config dir resolution itself and must point the raw
        // env var at a dir the shared guard cannot provide, as (file, fn).
        const CONFIG_DIR_PIN_EXEMPT: [(&str, &str); 1] = [
            // The config dir has to sit under a home dir the test then renames.
            (
                "function/mod.rs",
                "built_shims_resolve_paths_at_runtime_after_relocation",
            ),
        ];
        let config_dir_literal = format!("\"{}\"", utils::get_env_name("config_dir"));
        let scanned: Vec<(&Path, ScopedSource)> = sources
            .iter()
            .map(|&(path, source)| (path, scope_lines(source)))
            .collect();
        // A module declared in test-only code is test-only, and so is every
        // module a test-only file declares, down the tree.
        let mut test_only_files: HashSet<PathBuf> = HashSet::new();
        loop {
            let known = test_only_files.len();
            for (path, scoped) in &scanned {
                let file_is_test = test_only_files.contains(*path);
                for (module, in_test) in &scoped.modules {
                    if *in_test || file_is_test {
                        test_only_files.extend(module_files(path, module));
                    }
                }
            }
            if test_only_files.len() == known {
                break;
            }
        }
        let mut offences = Vec::new();
        for (relative, scoped) in &scanned {
            let relative: &Path = relative;
            let is = |name: &str| relative == Path::new(name);
            let test_only_file = test_only_files.contains(relative);
            let opens_set_call = |text: &str| {
                let text = text.trim_end();
                text.ends_with("set_var(") || text.ends_with("EnvVarGuard::set(")
            };
            for line in &scoped.path_attributes {
                offences.push(format!(
                    "src/{}:{line}: #[path] module; the scan cannot tell whether its file is test-only",
                    relative.display()
                ));
            }
            let mut previous_opens_set_call = false;
            for line in &scoped.lines {
                let in_test = test_only_file || line.in_test;
                let enclosing_fn = line.enclosing_fn.as_deref().unwrap_or_default();
                let text = line.text;
                let at = format!("src/{}:{}", relative.display(), line.number);
                if names_any(&line.code, &[RAW_WALK])
                    && !(is("config/memory.rs")
                        && (RAW_WALK_HOMES.contains(&enclosing_fn)
                            || (in_test && walk_names_a_ceiling(&line.code, RAW_WALK))))
                {
                    offences.push(format!(
                        "{at}: raw workspace memory walk; call memory::find_workspace_memory, or pass Some(ceiling) on one line in memory.rs tests"
                    ));
                }
                if text.contains(GUARD_DEFINITION) && !is("testing.rs") {
                    offences.push(format!(
                        "{at}: private config dir guard; use crate::testing::TestConfigDirGuard"
                    ));
                }
                let pins_config_dir = text.contains(CONFIG_DIR_ENV)
                    || (text.contains(&config_dir_literal)
                        && (previous_opens_set_call
                            || text.contains("set_var(")
                            || text.contains("EnvVarGuard::set(")));
                previous_opens_set_call = opens_set_call(text);
                let exempt_pin = CONFIG_DIR_PIN_EXEMPT
                    .iter()
                    .any(|&(file, test)| is(file) && enclosing_fn == test);
                if in_test && pins_config_dir && !is("testing.rs") && !exempt_pin {
                    offences.push(format!(
                        "{at}: test pins the config dir env by hand; use crate::testing::TestConfigDirGuard"
                    ));
                }
                let allowed_git_root_walk = GIT_ROOT_ALLOW
                    .iter()
                    .any(|&(file, allowed)| is(file) && enclosing_fn == allowed);
                if names_any(&line.code, &GIT_ROOT_WALKS) && !allowed_git_root_walk {
                    offences.push(format!(
                        "{at}: raw git root walk; call memory::find_workspace_git_root, or memory::find_git_root_under in tests"
                    ));
                }
                let allowed_init_memory_call = is("main.rs")
                    || (is("config/memory.rs")
                        && (in_test || INIT_MEMORY_HELPERS.contains(&enclosing_fn)));
                if names_any(&line.code, &INIT_MEMORY_HELPERS) && !allowed_init_memory_call {
                    offences.push(format!(
                        "{at}: unbounded --init-memory helper outside the explicit command in src/main.rs"
                    ));
                }
            }
        }
        offences
    }

    /// Each rule of `offences_in`, and the edge of each exemption, on sample
    /// sources: widening an exemption, or losing a rule, fails a case here.
    #[test]
    fn offences_in_holds_every_rule_on_sample_sources() {
        let in_test_fn = |name: &str, body: &str| {
            format!("#[cfg(test)]\nmod tests {{\n    fn {name}() {{\n        {body}\n    }}\n}}\n")
        };
        let in_test = |body: &str| in_test_fn("probe", body);
        let pin_config_dir = format!(
            "std::env::set_var(\"{}\", &dir);",
            utils::get_env_name("config_dir")
        );
        let exempt_pin_fn = "built_shims_resolve_paths_at_runtime_after_relocation";
        let init_in_probe = "fn probe() {\n    let _ = init_workspace_root(&dir);\n}\n";
        let multi_line_cfg = |predicate: &str| {
            format!(
                "#[cfg({predicate}(\n    test,\n    unix,\n))]\nmod tests {{\n{init_in_probe}}}\n"
            )
        };
        // (path relative to src/, source) for each file of a case.
        type Files = Vec<(&'static str, String)>;
        let cases: Vec<(&str, Files, usize)> = vec![
            (
                "a memory.rs test walk needs a ceiling",
                vec![("config/memory.rs", in_test("walk_workspace_memory(&start, None);"))],
                1,
            ),
            (
                "a memory.rs test walk with a ceiling",
                vec![(
                    "config/memory.rs",
                    in_test("walk_workspace_memory(&start, Some(&root));"),
                )],
                0,
            ),
            (
                "a ceiling the line cannot show",
                vec![(
                    "config/memory.rs",
                    in_test("walk_workspace_memory(\n            &start,\n            Some(&root),\n        );"),
                )],
                1,
            ),
            (
                "a point-free memory.rs test walk",
                vec![("config/memory.rs", in_test("let _ = a.and_then(walk_workspace_memory);"))],
                1,
            ),
            (
                "a shipped memory.rs walk outside its homes",
                vec![(
                    "config/memory.rs",
                    "fn shipped() {\n    walk_workspace_memory(&start, Some(&root));\n}\n".into(),
                )],
                1,
            ),
            (
                "the walk in its home",
                vec![(
                    "config/memory.rs",
                    "pub fn find_workspace_memory(cwd: &Path) {\n    walk_workspace_memory(&start, None)\n}\n".into(),
                )],
                0,
            ),
            (
                "a walk outside memory.rs, even with a ceiling",
                vec![("hooks.rs", in_test("walk_workspace_memory(&a, Some(&b));"))],
                1,
            ),
            (
                "a memory.rs test git root walk",
                vec![("config/memory.rs", in_test("let _ = find_git_root(&dir);"))],
                1,
            ),
            (
                "a git root walk in its own definition",
                vec![(
                    "config/memory.rs",
                    "fn find_workspace_git_root(cwd: &Path) {\n    find_git_root_within(&start, None)\n}\n".into(),
                )],
                0,
            ),
            (
                "an init helper outside memory.rs and main.rs",
                vec![("function/memory.rs", in_test("let _ = init_workspace_root(&dir);"))],
                1,
            ),
            (
                "an init helper in main.rs",
                vec![("main.rs", "fn main() {\n    init_workspace_root(&dir);\n}\n".into())],
                0,
            ),
            (
                "an init helper in a shipped memory.rs fn",
                vec![("config/memory.rs", init_in_probe.into())],
                1,
            ),
            (
                "an init helper in memory.rs test code",
                vec![("config/memory.rs", in_test("let _ = init_workspace_root(&dir);"))],
                0,
            ),
            (
                "a multi-line cfg that implies test",
                vec![("config/memory.rs", multi_line_cfg("all"))],
                0,
            ),
            (
                "a multi-line cfg that does not",
                vec![("config/memory.rs", multi_line_cfg("any"))],
                1,
            ),
            (
                "a file marked test-only by an inner cfg",
                vec![("config/memory.rs", format!("#![cfg(test)]\n{init_in_probe}"))],
                0,
            ),
            (
                "a private config dir guard",
                vec![("graph/state.rs", concat!("struct TestConfig", "DirGuard {}\n").into())],
                1,
            ),
            (
                "a test pinning the config dir by hand",
                vec![("hooks.rs", in_test(&pin_config_dir))],
                1,
            ),
            (
                "the exempt config dir pin",
                vec![("function/mod.rs", in_test_fn(exempt_pin_fn, &pin_config_dir))],
                0,
            ),
            (
                "the exempt test name in another file",
                vec![("hooks.rs", in_test_fn(exempt_pin_fn, &pin_config_dir))],
                1,
            ),
            (
                "a config dir pin in a file declared as a test module",
                vec![
                    ("hooks.rs", "#[cfg(test)]\nmod live;\n".into()),
                    ("hooks/live.rs", format!("fn probe() {{\n    {pin_config_dir}\n}}\n")),
                ],
                1,
            ),
            (
                "a config dir set in shipped code",
                vec![("hooks.rs", format!("fn probe() {{\n    {pin_config_dir}\n}}\n"))],
                0,
            ),
            (
                "a #[path] module",
                vec![("hooks.rs", "#[cfg(test)]\n#[path = \"other.rs\"]\nmod tests;\n".into())],
                1,
            ),
            (
                "a #[path] module behind cfg_attr",
                vec![(
                    "hooks.rs",
                    "#[cfg_attr(test, path = \"other.rs\")]\nmod tests;\n".into(),
                )],
                1,
            ),
            (
                "a config dir pin in a module declared inside a test module",
                vec![
                    ("hooks.rs", in_test("").replace("fn probe", "mod live;\n    fn probe")),
                    ("hooks/tests/live.rs", format!("fn probe() {{\n    {pin_config_dir}\n}}\n")),
                ],
                1,
            ),
            (
                "a config dir pin in a module a #![cfg(test)] file declares",
                vec![
                    ("hooks.rs", "#![cfg(test)]\nmod live;\n".into()),
                    ("hooks/live.rs", format!("fn probe() {{\n    {pin_config_dir}\n}}\n")),
                ],
                1,
            ),
            (
                "a config dir pin two module levels below a test module",
                vec![
                    ("hooks.rs", "#[cfg(test)]\nmod live;\n".into()),
                    ("hooks/live.rs", "mod deeper;\n".into()),
                    ("hooks/live/deeper.rs", format!("fn probe() {{\n    {pin_config_dir}\n}}\n")),
                ],
                1,
            ),
            (
                "a config dir pin split over two lines",
                vec![(
                    "hooks.rs",
                    in_test(&format!(
                        "std::env::set_var(\n            \"{}\",\n            &dir,\n        );",
                        utils::get_env_name("config_dir")
                    )),
                )],
                1,
            ),
            (
                "a config dir pin through the env name helper",
                vec![(
                    "hooks.rs",
                    in_test(concat!("let _ = std::env::set_var(utils::get_env_name(\"config", "_dir\"), &dir);")),
                )],
                1,
            ),
            (
                "a config dir pin through EnvVarGuard",
                vec![(
                    "hooks.rs",
                    in_test(&format!(
                        "let _g = EnvVarGuard::set(\"{}\", &dir);",
                        utils::get_env_name("config_dir")
                    )),
                )],
                1,
            ),
            (
                "the shared guard's own config dir pin",
                vec![("testing.rs", in_test(&pin_config_dir))],
                0,
            ),
            (
                "the shared guard's own definition",
                vec![("testing.rs", concat!("struct TestConfig", "DirGuard {}\n").into())],
                0,
            ),
            (
                "the walk's own definition",
                vec![(
                    "config/memory.rs",
                    "fn walk_workspace_memory(start: &Path, ceiling: Option<&Path>) {\n    walk_workspace_memory(start, None);\n}\n".into(),
                )],
                0,
            ),
        ];
        let mismatches: Vec<String> = cases
            .into_iter()
            .filter_map(|(case, files, expected)| {
                let files: Vec<(&Path, &str)> = files
                    .iter()
                    .map(|(path, source)| (Path::new(*path), source.as_str()))
                    .collect();
                let offences = offences_in(&files);
                (offences.len() != expected)
                    .then(|| format!("{case}: expected {expected}, got {offences:#?}"))
            })
            .collect();
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    /// Whether every call of `walk` in `code` closes on the line and passes
    /// `Some(..)` as its second argument, the ceiling. Only the syntax is
    /// checked, not what the ceiling bounds.
    fn walk_names_a_ceiling(code: &str, walk: &str) -> bool {
        let tokens = tokens(code);
        let calls: Vec<usize> = (0..tokens.len()).filter(|&i| tokens[i] == walk).collect();
        calls.iter().all(|&at| {
            if tokens.get(at + 1) != Some(&"(") {
                return false;
            }
            let Some(close) = closing_paren(&tokens, at + 1) else {
                return false;
            };
            let mut depth = 0usize;
            let second = tokens[at + 2..close].iter().position(|&token| {
                match token {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => depth = depth.saturating_sub(1),
                    "," if depth == 0 => return true,
                    _ => {}
                }
                false
            });
            second
                .is_some_and(|comma| tokens[at + 2 + comma + 1..close].starts_with(&["Some", "("]))
        })
    }

    #[test]
    fn names_any_matches_identifier_tokens_outside_literals_and_comments() {
        const WALKS: [&str; 2] = ["find_git_root", "find_git_root_within"];
        for line in [
            "let root = cwd.and_then(memory::find_git_root);",
            "use crate::config::memory::find_git_root as root;",
            "let r = find_git_root_within(a, None);",
        ] {
            assert!(names_any(&code_only(line), &WALKS), "{line}");
        }
        for line in [
            "let s = \"find_git_root\";",
            "// find_git_root here",
            "find_git_root_under(a, b)",
        ] {
            assert!(!names_any(&code_only(line), &WALKS), "{line}");
        }
    }

    #[test]
    fn scope_lines_tracks_test_items_and_the_enclosing_fn() {
        let source = concat!(
            "fn shipped() {\n",
            "    let s = \"} fn fake() {\";\n",
            "    let c = '{';\n",
            "}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn probe() {\n",
            "        let r = r#\"{\"#;\n",
            "    }\n",
            "}\n",
            "#[cfg(test)]\n",
            "mod live;\n",
            "fn after<'a>(x: &'a str) {\n",
            "}\n",
            "struct Holder {\n",
            "    #[cfg(test)]\n",
            "    probe: Option<u8>,\n",
            "}\n",
            "impl Holder {\n",
            "    fn shipped_too(a: u8, b: u8) {\n",
            "        Self {\n",
            "            #[cfg(test)]\n",
            "            probe: None\n",
            "        };\n",
            "    }\n",
            "}\n",
            "#[cfg(test)]\n",
            "#[derive(Clone, Copy)]\n",
            "impl<A, B> Holder {\n",
            "    fn probe_too<C, D>(c: C, d: D) {\n",
            "    }\n",
            "}\n",
            "#[cfg(all(test, unix))]\n",
            "mod m {\n",
            "    fn a() {}\n",
            "}\n",
            "#[cfg(all(test, not(target_env = \"musl\")))]\n",
            "mod n {\n",
            "}\n",
            "#[cfg(any(test, unix))]\n",
            "fn b() {\n",
            "}\n",
            "#[cfg(any(test, all(test, unix)))]\n",
            "fn c() {\n",
            "}\n",
            "#[cfg(not(test))]\n",
            "fn d() {\n",
            "}\n",
            "#[cfg(test)]\n",
            "fn helper(buf: [u8; 4]) {\n",
            "}\n",
        );
        let scoped = scope_lines(source);
        let scopes: Vec<(bool, Option<&str>)> = scoped
            .lines
            .iter()
            .map(|line| (line.in_test, line.enclosing_fn.as_deref()))
            .collect();
        assert_eq!(
            scopes,
            [
                (false, Some("shipped")),
                (false, Some("shipped")),
                (false, Some("shipped")),
                (false, None),
                (false, None),
                (true, None),
                (true, Some("probe")),
                (true, Some("probe")),
                (true, None),
                (false, None),
                (false, None),
                (false, None),
                (false, Some("after")),
                (false, None),
                (false, None),
                (false, None),
                (false, None),
                (false, None),
                (false, None),
                (false, Some("shipped_too")),
                (false, Some("shipped_too")),
                (false, Some("shipped_too")),
                (false, Some("shipped_too")),
                (false, Some("shipped_too")),
                (false, None),
                (false, None),
                (false, None),
                (false, None),
                (true, None),
                (true, Some("probe_too")),
                (true, None),
                (false, None),
                (false, None),
                (true, None),
                (true, None),
                (false, None),
                (false, None),
                (true, None),
                (false, None),
                (false, None),
                (false, Some("b")),
                (false, None),
                (false, None),
                (true, Some("c")),
                (false, None),
                (false, None),
                (false, Some("d")),
                (false, None),
                (false, None),
                (true, Some("helper")),
                (false, None),
            ]
        );
        assert_eq!(scoped.modules, [("live".to_string(), true)]);
    }

    #[test]
    fn cfg_implies_test_only_for_predicates_no_shipped_build_satisfies() {
        for predicate in [
            "test",
            " test ",
            "all(test, unix)",
            "all(unix, all(feature = \"a,b\", test))",
            "all(test, not(target_env = \"musl\"))",
            "any(test, all(test, unix))",
            "any(test,)",
        ] {
            assert!(cfg_implies_test(predicate), "{predicate}");
        }
        for predicate in [
            "unix",
            "tests",
            "feature = \"test\"",
            "not(test)",
            "any(test, unix)",
            "any()",
            "all()",
            "all(unix, not(test))",
            "any(test, feature = \"x,test\")",
        ] {
            assert!(!cfg_implies_test(predicate), "{predicate}");
        }
    }

    /// One source line with the scope it sits in.
    struct ScopedLine<'a> {
        number: usize,
        text: &'a str,
        /// `text` with literals and comments blanked.
        code: String,
        in_test: bool,
        enclosing_fn: Option<String>,
    }

    struct ScopedSource<'a> {
        lines: Vec<ScopedLine<'a>>,
        /// Each `mod name;` declaration as its path below the file's own
        /// module (`name`, or `outer/name` inside an inline `mod outer`),
        /// and whether it sits in test-only code.
        modules: Vec<(String, bool)>,
        /// Line numbers of `#[path = ..]` attributes, plain or inside a
        /// `cfg_attr`, whose module file the scan does not resolve.
        path_attributes: Vec<usize>,
    }

    #[derive(Clone, Default)]
    struct Scope {
        in_test: bool,
        enclosing_fn: Option<String>,
        /// Open `(` and `[` inside this brace scope.
        nesting: usize,
        /// The inline `mod` blocks this scope sits in, outermost first.
        module_path: Vec<String>,
    }

    /// Tracks brace depth over `source` with literals and comments blanked,
    /// recording for each line the scope in force at its end. A `#[cfg(..)]`
    /// (which may span lines) whose predicate implies `test`, or a `fn`
    /// name, applies to the next `{` and is dropped at a `}` or a `;`
    /// outside brackets. Outside an item header, an attribute is also
    /// dropped at a top-level `,`, which ends the struct field or literal
    /// field it annotated. An inner `#![cfg(..)]` that implies `test` marks
    /// the rest of the scope it sits in, the whole file at its top.
    fn scope_lines(source: &str) -> ScopedSource<'_> {
        const CFG_OPEN: [&str; 4] = ["#", "[", "cfg", "("];
        const INNER_CFG_OPEN: [&str; 5] = ["#", "!", "[", "cfg", "("];
        const PATH_OPEN: [&str; 4] = ["#", "[", "path", "="];
        const CFG_ATTR_OPEN: [&str; 4] = ["#", "[", "cfg_attr", "("];
        const ITEM_KEYWORDS: [&str; 7] = ["fn", "impl", "struct", "enum", "trait", "mod", "union"];
        let code = code_only(source);
        let code_lines: Vec<&str> = code.lines().collect();
        let (token_lines, tokens): (Vec<usize>, Vec<&str>) = code_lines
            .iter()
            .enumerate()
            .flat_map(|(index, code)| tokens(code).into_iter().map(move |t| (index, t)))
            .unzip();
        let mut scopes = vec![Scope::default()];
        let mut pending_test = false;
        let mut pending_item = false;
        let mut pending_fn = None;
        let mut pending_mod = None;
        let mut modules = Vec::new();
        let mut path_attributes = Vec::new();
        let mut lines = Vec::new();
        let mut texts = source.lines().zip(&code_lines).enumerate();
        // Records every line before `line` with the scope now in force,
        // which is the scope at the end of each of them: their tokens are
        // all processed by the time a later line's token is reached.
        let mut record_until = |line: usize, scope: &Scope| {
            while lines.len() < line {
                let Some((index, (text, code))) = texts.next() else {
                    break;
                };
                lines.push(ScopedLine {
                    number: index + 1,
                    text,
                    code: code.to_string(),
                    in_test: scope.in_test,
                    enclosing_fn: scope.enclosing_fn.clone(),
                });
            }
        };
        let mut k = 0;
        while k < tokens.len() {
            record_until(token_lines[k], scopes.last().unwrap());
            let inner = tokens[k..].starts_with(&INNER_CFG_OPEN);
            let open = if inner {
                INNER_CFG_OPEN.len()
            } else {
                CFG_OPEN.len()
            };
            if (inner || tokens[k..].starts_with(&CFG_OPEN))
                && let Some(close) = closing_paren(&tokens, k + open - 1)
                && tokens.get(close + 1) == Some(&"]")
            {
                let implies_test = cfg_implies_test(&tokens[k + open..close].join(" "));
                if inner {
                    scopes.last_mut().unwrap().in_test |= implies_test;
                } else {
                    pending_test |= implies_test;
                }
                k = close + 2;
                continue;
            }
            if tokens[k..].starts_with(&PATH_OPEN) {
                path_attributes.push(token_lines[k] + 1);
            }
            if tokens[k..].starts_with(&CFG_ATTR_OPEN)
                && let Some(close) = closing_paren(&tokens, k + CFG_ATTR_OPEN.len() - 1)
                && tokens[k..close]
                    .windows(2)
                    .any(|pair| pair == ["path", "="])
            {
                path_attributes.push(token_lines[k] + 1);
            }
            let next_ident = tokens.get(k + 1).filter(|t| is_ident(t));
            if ITEM_KEYWORDS.contains(&tokens[k])
                && (next_ident.is_some() || tokens.get(k + 1) == Some(&"<"))
            {
                pending_item = true;
            }
            match tokens[k] {
                "fn" if next_ident.is_some() => pending_fn = next_ident.map(|t| t.to_string()),
                "mod" if next_ident.is_some() => pending_mod = next_ident.map(|t| t.to_string()),
                "{" => {
                    let outer = scopes.last().unwrap();
                    let mut module_path = outer.module_path.clone();
                    module_path.extend(pending_mod.take());
                    let scope = Scope {
                        in_test: outer.in_test || pending_test,
                        enclosing_fn: pending_fn.take().or_else(|| outer.enclosing_fn.clone()),
                        nesting: 0,
                        module_path,
                    };
                    scopes.push(scope);
                    pending_test = false;
                    pending_item = false;
                }
                "}" => {
                    if scopes.len() > 1 {
                        scopes.pop();
                    }
                    pending_test = false;
                    pending_item = false;
                    pending_fn = None;
                    pending_mod = None;
                }
                ";" if scopes.last().unwrap().nesting == 0 => {
                    if let Some(name) = pending_mod.take() {
                        let scope = scopes.last().unwrap();
                        let mut path = scope.module_path.clone();
                        path.push(name);
                        modules.push((path.join("/"), pending_test || scope.in_test));
                    }
                    pending_test = false;
                    pending_item = false;
                    pending_fn = None;
                    pending_mod = None;
                }
                "(" | "[" => scopes.last_mut().unwrap().nesting += 1,
                ")" | "]" => {
                    let scope = scopes.last_mut().unwrap();
                    scope.nesting = scope.nesting.saturating_sub(1);
                }
                "," if !pending_item && scopes.last().unwrap().nesting == 0 => {
                    pending_test = false;
                    pending_fn = None;
                    pending_mod = None;
                }
                _ => {}
            }
            k += 1;
        }
        record_until(usize::MAX, scopes.last().unwrap());
        ScopedSource {
            lines,
            modules,
            path_attributes,
        }
    }

    /// The index of the `)` matching the `(` at `open`.
    fn closing_paren(tokens: &[&str], open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (i, token) in tokens.iter().enumerate().skip(open) {
            match *token {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Whether code under `#[cfg(predicate)]` is compiled only for tests:
    /// `test` itself, an `all` with an argument that implies it, or an `any`
    /// whose every argument does. `not(..)` and every other predicate can
    /// hold in a shipped build.
    fn cfg_implies_test(predicate: &str) -> bool {
        let predicate = predicate.trim();
        if predicate == "test" {
            return true;
        }
        let Some((operator, rest)) = predicate.split_once('(') else {
            return false;
        };
        let Some(arguments) = rest.trim_end().strip_suffix(')') else {
            return false;
        };
        let arguments = cfg_arguments(arguments);
        match operator.trim() {
            "all" => arguments.into_iter().any(cfg_implies_test),
            "any" => !arguments.is_empty() && arguments.into_iter().all(cfg_implies_test),
            _ => false,
        }
    }

    /// The comma-separated arguments of a cfg operator, split only at commas
    /// outside nested parentheses and string literals.
    fn cfg_arguments(arguments: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut start = 0;
        for (i, c) in arguments.char_indices() {
            if in_string {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    out.push(&arguments[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        out.push(&arguments[start..]);
        out.retain(|argument| !argument.trim().is_empty());
        out
    }

    /// Identifiers as one token each; every other non-space char alone.
    fn tokens(code: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut chars = code.char_indices().peekable();
        while let Some((start, c)) = chars.next() {
            if c.is_whitespace() {
                continue;
            }
            let mut end = start + c.len_utf8();
            if c.is_alphanumeric() || c == '_' {
                while let Some(&(at, next)) = chars.peek() {
                    if !(next.is_alphanumeric() || next == '_') {
                        break;
                    }
                    end = at + next.len_utf8();
                    chars.next();
                }
            }
            out.push(&code[start..end]);
        }
        out
    }

    /// Whether `code` holds any of `names` as a whole identifier token.
    fn names_any(code: &str, names: &[&str]) -> bool {
        tokens(code).iter().any(|token| names.contains(token))
    }

    fn is_ident(token: &str) -> bool {
        token
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
    }

    /// `source` with string and char literals and comments turned into
    /// spaces, newlines kept, so braces and keywords inside them are not
    /// read as code.
    fn code_only(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < chars.len() {
            match literal_or_comment_end(&chars, i) {
                Some(end) => {
                    out.extend(
                        chars[i..end]
                            .iter()
                            .map(|&c| if c == '\n' { c } else { ' ' }),
                    );
                    i = end;
                }
                None => {
                    out.push(chars[i]);
                    i += 1;
                }
            }
        }
        out
    }

    /// The index just past the literal or comment starting at `i`, if one
    /// does. A `'` not closed two chars later is a lifetime.
    fn literal_or_comment_end(chars: &[char], i: usize) -> Option<usize> {
        let at = |j: usize| chars.get(j).copied();
        let first_from = |from: usize, found: &dyn Fn(usize) -> bool| {
            (from..chars.len())
                .find(|&j| found(j))
                .unwrap_or(chars.len())
        };
        let after_ident = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        match chars[i] {
            '/' if at(i + 1) == Some('/') => Some(first_from(i, &|j| chars[j] == '\n')),
            '/' if at(i + 1) == Some('*') => Some(
                (first_from(i + 2, &|j| chars[j] == '*' && at(j + 1) == Some('/')) + 2)
                    .min(chars.len()),
            ),
            '"' => Some(quoted_end(chars, i + 1, None)),
            'r' if !after_ident => {
                let hashes = chars[i + 1..].iter().take_while(|&&c| c == '#').count();
                (at(i + 1 + hashes) == Some('"'))
                    .then(|| quoted_end(chars, i + 2 + hashes, Some(hashes)))
            }
            '\'' if at(i + 1) == Some('\\') => {
                Some((first_from(i + 3, &|j| chars[j] == '\'') + 1).min(chars.len()))
            }
            '\'' if at(i + 2) == Some('\'') => Some(i + 3),
            _ => None,
        }
    }

    /// The index just past the closing quote of a string whose body starts
    /// at `from`; `raw_hashes` is set for a raw string, which has no escapes.
    fn quoted_end(chars: &[char], from: usize, raw_hashes: Option<usize>) -> usize {
        let mut j = from;
        while j < chars.len() {
            match (chars[j], raw_hashes) {
                ('\\', None) => j += 2,
                ('"', None) => return j + 1,
                ('"', Some(n)) if chars[j + 1..].iter().take_while(|&&c| c == '#').count() >= n => {
                    return j + 1 + n;
                }
                _ => j += 1,
            }
        }
        chars.len()
    }

    /// The files a `mod name;` in `parent` may load; `module` is the
    /// declaration's `/`-joined path from `ScopedSource::modules`.
    fn module_files(parent: &Path, module: &str) -> [PathBuf; 2] {
        let mut dir = match parent.file_stem().and_then(|stem| stem.to_str()) {
            Some("mod" | "main" | "lib") => parent.parent().unwrap().to_path_buf(),
            _ => parent.with_extension(""),
        };
        let (inline, name) = module.rsplit_once('/').unwrap_or(("", module));
        dir.extend(inline.split('/').filter(|segment| !segment.is_empty()));
        [
            dir.join(format!("{name}.rs")),
            dir.join(name).join("mod.rs"),
        ]
    }

    fn rust_sources(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
        out
    }
}
