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
}

impl Drop for TestConfigDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.workspace_root);
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

    /// Workspace memory lookups go through the pinned entry point in
    /// src/config/memory.rs, git-root walks outside it are bounded, and tests
    /// pin the config dir only through the shared guard, so no test can reach
    /// the developer's real workspace memory or config dir through a raw
    /// ancestor walk or a private guard that skips the pins. Each line is
    /// judged by the scope it sits in (a `#[cfg(test)]` item, a file declared
    /// as a `#[cfg(test)]` module, the innermost `fn`), not by its file. The
    /// needles are split so this file's own source does not match them.
    #[test]
    fn no_source_file_bypasses_the_workspace_memory_pin() {
        const RAW_WALK: &str = concat!("walk_workspace", "_memory");
        const RAW_WALK_HOMES: [&str; 2] = [RAW_WALK, "find_workspace_memory"];
        const GUARD_DEFINITION: &str = concat!("struct TestConfig", "DirGuard");
        const CONFIG_DIR_ENV: &str = concat!("env_name(\"config", "_dir\")");
        const GIT_ROOT_WALKS: [&str; 2] = [
            concat!("find_git", "_root("),
            concat!("find_git", "_root_within("),
        ];
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
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let sources: Vec<(PathBuf, String)> = rust_sources(&src)
            .into_iter()
            .map(|path| {
                let source = fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                (path, source)
            })
            .collect();
        let scanned: Vec<(&Path, ScopedSource)> = sources
            .iter()
            .map(|(path, source)| (path.as_path(), scope_lines(source)))
            .collect();
        let test_only_files: HashSet<PathBuf> = scanned
            .iter()
            .flat_map(|(path, scoped)| {
                scoped
                    .test_modules
                    .iter()
                    .flat_map(|name| module_files(path, name))
            })
            .collect();
        let mut offences = Vec::new();
        for (path, scoped) in &scanned {
            let relative = path.strip_prefix(&src).unwrap();
            let is = |name: &str| relative == Path::new(name);
            let test_only_file = test_only_files.contains(*path);
            let opens_set_call = |text: &str| {
                let text = text.trim_end();
                text.ends_with("set_var(") || text.ends_with("EnvVarGuard::set(")
            };
            let mut previous_opens_set_call = false;
            for line in &scoped.lines {
                let in_test = test_only_file || line.in_test;
                let enclosing_fn = line.enclosing_fn.as_deref().unwrap_or_default();
                let text = line.text;
                let at = format!("src/{}:{}", relative.display(), line.number);
                if text.contains(RAW_WALK)
                    && !(is("config/memory.rs")
                        && (in_test || RAW_WALK_HOMES.contains(&enclosing_fn)))
                {
                    offences.push(format!(
                        "{at}: raw workspace memory walk; call memory::find_workspace_memory"
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
                let allowed_git_root_walk = (is("config/memory.rs") && in_test)
                    || GIT_ROOT_ALLOW
                        .iter()
                        .any(|&(file, allowed)| is(file) && enclosing_fn == allowed);
                if GIT_ROOT_WALKS.iter().any(|walk| text.contains(walk)) && !allowed_git_root_walk {
                    offences.push(format!(
                        "{at}: raw git root walk; call memory::find_workspace_git_root, or memory::find_git_root_under in tests"
                    ));
                }
            }
        }
        assert!(offences.is_empty(), "{}", offences.join("\n"));
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
        assert_eq!(scoped.test_modules, ["live"]);
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
        in_test: bool,
        enclosing_fn: Option<String>,
    }

    struct ScopedSource<'a> {
        lines: Vec<ScopedLine<'a>>,
        /// Names from `#[cfg(test)] mod name;` declarations.
        test_modules: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct Scope {
        in_test: bool,
        enclosing_fn: Option<String>,
        /// Open `(` and `[` inside this brace scope.
        nesting: usize,
    }

    /// Tracks brace depth over `source` with literals and comments blanked,
    /// recording for each line the scope in force at its end. A `#[cfg(..)]`
    /// whose predicate implies `test`, or a `fn` name, applies to the next
    /// `{` and is dropped at a `}` or a `;` outside brackets. Outside an
    /// item header, an attribute is also dropped at a top-level `,`, which
    /// ends the struct field or literal field it annotated.
    fn scope_lines(source: &str) -> ScopedSource<'_> {
        const CFG_OPEN: [&str; 4] = ["#", "[", "cfg", "("];
        const ITEM_KEYWORDS: [&str; 7] = ["fn", "impl", "struct", "enum", "trait", "mod", "union"];
        let code = code_only(source);
        let mut scopes = vec![Scope::default()];
        let mut pending_test = false;
        let mut pending_item = false;
        let mut pending_fn = None;
        let mut pending_mod = None;
        let mut test_modules = Vec::new();
        let mut lines = Vec::new();
        for (index, (text, code)) in source.lines().zip(code.lines()).enumerate() {
            let tokens = tokens(code);
            let mut k = 0;
            while k < tokens.len() {
                if tokens[k..].starts_with(&CFG_OPEN)
                    && let Some(close) = closing_paren(&tokens, k + CFG_OPEN.len() - 1)
                    && tokens.get(close + 1) == Some(&"]")
                {
                    let predicate = tokens[k + CFG_OPEN.len()..close].join(" ");
                    pending_test |= cfg_implies_test(&predicate);
                    k = close + 2;
                    continue;
                }
                let next_ident = tokens.get(k + 1).filter(|t| is_ident(t));
                if ITEM_KEYWORDS.contains(&tokens[k])
                    && (next_ident.is_some() || tokens.get(k + 1) == Some(&"<"))
                {
                    pending_item = true;
                }
                match tokens[k] {
                    "fn" if next_ident.is_some() => pending_fn = next_ident.map(|t| t.to_string()),
                    "mod" if next_ident.is_some() => {
                        pending_mod = next_ident.map(|t| t.to_string())
                    }
                    "{" => {
                        let outer = scopes.last().unwrap();
                        let scope = Scope {
                            in_test: outer.in_test || pending_test,
                            enclosing_fn: pending_fn.take().or_else(|| outer.enclosing_fn.clone()),
                            nesting: 0,
                        };
                        scopes.push(scope);
                        pending_test = false;
                        pending_item = false;
                        pending_mod = None;
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
                        if let Some(name) = pending_mod.take().filter(|_| pending_test) {
                            test_modules.push(name);
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
            let scope = scopes.last().unwrap();
            lines.push(ScopedLine {
                number: index + 1,
                text,
                in_test: scope.in_test,
                enclosing_fn: scope.enclosing_fn.clone(),
            });
        }
        ScopedSource {
            lines,
            test_modules,
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

    /// The files a `mod name;` in `parent` may load.
    fn module_files(parent: &Path, name: &str) -> [PathBuf; 2] {
        let dir = match parent.file_stem().and_then(|stem| stem.to_str()) {
            Some("mod" | "main" | "lib") => parent.parent().unwrap().to_path_buf(),
            _ => parent.with_extension(""),
        };
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
