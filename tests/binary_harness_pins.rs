//! Pins the environment every integration harness hands the real binary:
//! the built `coyote` is named in one place, `tests/support/bin.rs`, so every
//! spawn goes through `support::bin::coyote_command` (or `isolate` for a
//! wrapper such as `script`). That pins the config, cache and home dirs to
//! the harness's own, pins the temp dir to whatever the harness passes (most
//! pass the inherited one, so it is stated explicitly rather than isolated),
//! and clears every inherited `COYOTE_*` variable, the unit-test
//! workspace-memory root override among them, so nothing a test run spawns
//! lands in the developer's real config or cache or picks up their overrides.
//!
//! Also pins the support helpers themselves: `support::repo::read_path` folds
//! CRLF to LF, and `support::bin::isolate_from` sets exactly the pinned vars,
//! removes inherited `COYOTE_*` keys only, and lets later vars win.

mod support;

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use support::bin::{Dirs, DryRunConfig, PINNED_VARS, coyote_command, isolate_from};

// Spelled in two halves so this file's own source does not count as a
// spawn site when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const SUPPORT_BIN: &str = "tests/support/bin.rs";
// Split for the same reason: a bare `Command` around the binary path skips
// the isolation coyote_command applies.
const BARE_SPAWNS: [&str; 8] = [
    concat!("Command::new(support::bin::", "path()"),
    concat!("Command::new(bin::", "path()"),
    concat!("Command::new(", "path())"),
    concat!("Command::new(", "binary)"),
    concat!("Command::new(&", "binary)"),
    concat!("Command::new(&support::bin::", "path()"),
    concat!("Command::new(&bin::", "path()"),
    concat!("Command::new(&", "path())"),
];
// Matches both the `support::bin::` and the `bin::` spelling exactly once.
const PATH_CALL: &str = concat!("bin::", "path()");
// Every sanctioned use of the binary path outside tests/support/bin.rs, with
// its exact count. None of them spawns the binary in a bare Command.
const PATH_USES: [(&str, usize, &str); 4] = [
    (
        "tests/binary_harness_pins.rs",
        1,
        "the get_program comparison in the coyote_command pin",
    ),
    (
        "tests/graph_session_block_binary.rs",
        2,
        "the binary dir for the PATH pin and for the reported-PATH check",
    ),
    (
        "tests/graph_session_repl_binary.rs",
        1,
        "the binary dir for the PATH pin",
    ),
    (
        SBX_ARGV_FORWARDING,
        SBX_ARGV_USES,
        "argv for the script(1) wrapper, whose command goes through isolate",
    ),
];
const SBX_ARGV_FORWARDING: &str = "tests/sbx_argv_forwarding_binary.rs";
const SBX_ARGV_USES: usize = 2;
const GRAPH_SESSION_HARNESSES: [&str; 2] = [
    "tests/graph_session_block_binary.rs",
    "tests/graph_session_repl_binary.rs",
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("tests dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `tests/**/*.rs` file as its crate-relative `/`-separated path and
/// its text, sorted by path.
fn test_sources() -> Vec<(String, String)> {
    let root = support::repo::root();
    let mut paths = Vec::new();
    rust_sources(&root.join("tests"), &mut paths);
    let mut sources: Vec<(String, String)> = paths
        .iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, support::repo::read_path(path))
        })
        .collect();
    sources.sort();
    sources
}

#[test]
fn every_spawn_goes_through_coyote_command_which_pins_the_dirs_and_clears_inherited_coyote_vars() {
    let sources = test_sources();
    let naming: Vec<&str> = sources
        .iter()
        .filter(|(_, text)| text.contains(SPAWN_SITE))
        .map(|(rel, _)| rel.as_str())
        .collect();
    assert_eq!(
        naming,
        [SUPPORT_BIN],
        "only {SUPPORT_BIN} may name the built binary; every spawn goes through \
         support::bin::coyote_command, which pins the config, cache and home dirs, \
         pins the temp dir to what the caller passes, and clears every inherited \
         COYOTE_* variable"
    );

    let bare: Vec<&str> = sources
        .iter()
        .filter(|(rel, text)| {
            rel != SUPPORT_BIN && BARE_SPAWNS.iter().any(|spawn| text.contains(spawn))
        })
        .map(|(rel, _)| rel.as_str())
        .collect();
    assert!(
        bare.is_empty(),
        "these files wrap the binary path in a bare Command, skipping the dir pins and \
         COYOTE_* clearing; spawn through support::bin::coyote_command instead: {bare:?}"
    );

    for (rel, text) in sources.iter().filter(|(rel, _)| rel != SUPPORT_BIN) {
        let uses = text.matches(PATH_CALL).count();
        let allowed = PATH_USES
            .iter()
            .find(|(file, _, _)| file == rel)
            .map_or(0, |(_, count, _)| *count);
        assert_eq!(
            uses, allowed,
            "{rel} uses the binary path {uses} times but PATH_USES sanctions {allowed}; \
             spawn through support::bin::coyote_command, or list a non-spawn use in \
             PATH_USES with its reason"
        );
    }

    let wrapped = sources
        .iter()
        .find(|(rel, _)| rel == SBX_ARGV_FORWARDING)
        .map_or(0, |(_, text)| text.matches("isolate(").count());
    assert!(
        wrapped >= SBX_ARGV_USES,
        "{SBX_ARGV_FORWARDING} feeds the binary path into {SBX_ARGV_USES} script(1) argvs but \
         has only {wrapped} isolate calls; each wrapper command must go through isolate"
    );
}

#[test]
fn every_graph_session_harness_spawn_pins_path() {
    let root = support::repo::root();
    for rel in GRAPH_SESSION_HARNESSES {
        let text = support::repo::read_path(&root.join(rel));
        let spawns = text.matches("coyote_command(").count() + text.matches("isolate(").count();
        let pins = text.matches(".env(\"PATH\", pinned_path(").count();
        assert!(
            spawns > 0 && spawns == pins,
            "{rel}: {spawns} coyote_command or isolate spawns but {pins} PATH pins; a \
             graph-session spawn must run with only the binary dir and system shells on PATH"
        );
    }
}

#[test]
fn isolate_from_pins_the_dirs_removes_inherited_coyote_keys_only_and_lets_later_vars_win() {
    let dirs = distinct_dirs();
    let mut command = Command::new("true");
    isolate_from(&mut command, &dirs, ["COYOTE_ISOLATION_PROBE", "PATH"]);
    command.env("COYOTE_LATER", "1");
    let envs = envs_of(&command);

    for (key, dir) in expected_pins(&dirs) {
        assert_eq!(
            envs.get(OsStr::new(key)),
            Some(&Some(dir.as_os_str().to_os_string())),
            "isolate_from must set {key} from Dirs"
        );
    }
    let mut set: Vec<&OsStr> = envs
        .iter()
        .filter(|(key, value)| value.is_some() && key.as_os_str() != "COYOTE_LATER")
        .map(|(key, _)| key.as_os_str())
        .collect();
    let mut listed: Vec<&OsStr> = PINNED_VARS.iter().map(OsStr::new).collect();
    set.sort_unstable();
    listed.sort_unstable();
    assert_eq!(
        set, listed,
        "PINNED_VARS must name exactly the vars isolate_from sets"
    );
    assert_eq!(
        envs.get(OsStr::new("COYOTE_ISOLATION_PROBE")),
        Some(&None),
        "isolate_from must remove an inherited COYOTE_* key"
    );
    assert_eq!(
        envs.get(OsStr::new("PATH")),
        None,
        "isolate_from must leave an inherited non-COYOTE_ key alone"
    );
    assert_eq!(
        envs.get(OsStr::new("COYOTE_LATER")),
        Some(&Some(OsString::from("1"))),
        "a variable set after isolate_from must reach the child"
    );
}

#[test]
fn coyote_command_targets_the_built_binary_with_the_dir_pins() {
    let dirs = distinct_dirs();
    let command = coyote_command(&dirs);
    assert_eq!(command.get_program(), support::bin::path());
    let envs = envs_of(&command);
    for (key, dir) in expected_pins(&dirs) {
        assert_eq!(
            envs.get(OsStr::new(key)),
            Some(&Some(dir.as_os_str().to_os_string())),
            "coyote_command must set {key} from Dirs"
        );
    }
}

fn distinct_dirs() -> Dirs {
    let base = env::temp_dir().join("coyote-isolate-from");
    Dirs {
        config: base.join("config"),
        cache: base.join("cache"),
        home: base.join("home"),
        temp: base.join("temp"),
    }
}

fn expected_pins(dirs: &Dirs) -> [(&'static str, &PathBuf); 7] {
    [
        ("COYOTE_CONFIG_DIR", &dirs.config),
        ("COYOTE_CACHE_DIR", &dirs.cache),
        ("HOME", &dirs.home),
        ("USERPROFILE", &dirs.home),
        ("TMPDIR", &dirs.temp),
        ("TMP", &dirs.temp),
        ("TEMP", &dirs.temp),
    ]
}

fn envs_of(command: &Command) -> HashMap<OsString, Option<OsString>> {
    command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
        .collect()
}

#[test]
fn read_path_folds_crlf_to_lf() {
    let dir = env::temp_dir().join(format!("coyote-read-path-{}", process::id()));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("crlf.txt");
    fs::write(&file, "one\r\ntwo\r\n").unwrap();

    let text = support::repo::read_path(&file);
    fs::remove_dir_all(&dir).unwrap();

    assert_eq!(text, "one\ntwo\n");
}

// Each site's current DryRunConfig chain against the literal config.yaml
// text that site wrote before it moved to DryRunConfig, both filled in with
// the same placeholder values.
#[test]
fn dry_run_config_renders_each_migrated_fixture_byte_for_byte() {
    let vault_pass = Path::new("/fixture/vault-pass");
    let started = "touch /fixture/STARTED";
    let completed = "touch /fixture/COMPLETED";
    let failed = "touch /fixture/FAILED";
    let interrupted = "touch /fixture/INTERRUPTED";
    let stopped = "touch /fixture/STOPPED";
    let relay_port = 4242;
    let envoy_block = "  envoy_memory:\n    enabled: true\n";
    let mesh_enabled = true;
    let interfaces = "    - {type: private, host: 127.0.0.1, port: 4242}\n";
    let extra_config = "  agent.extra:\n    - name: mark\nmodel_extra: true\n";
    let prefix = "command_prefix: '/'";

    let plain_vault = DryRunConfig::default()
        .vault_password_file(vault_pass)
        .render();
    let plain = DryRunConfig::default().render();
    let command_prefix = |prefix_line: Option<&str>| {
        let mut config = DryRunConfig::default();
        if let Some(line) = prefix_line {
            config = config.setting(line);
        }
        config
            .vault_password_file(vault_pass)
            .quoted_api_key()
            .append("memory: false\nstream: false\n")
            .render()
    };
    let command_prefix_old = |prefix_line: Option<&str>| {
        let prefix_line = prefix_line.map(|l| format!("{l}\n")).unwrap_or_default();
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             {prefix_line}\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             memory: false\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
        )
    };

    let sites: [(&str, String, String); 10] = [
        ("envoy_repl Probe::new", plain_vault.clone(), {
            const DRYRUN_CONFIG_TAIL: &str = r#"clients:
  - type: openai
    name: dryrun
    auth: none
    api_key: unused
    models:
      - name: dry-model
        max_input_tokens: 100000
        supports_function_calling: true
save: false
"#;
            format!(
                "model: dryrun:dry-model\ndry_run: true\nvault_password_file: '{}'\n{DRYRUN_CONFIG_TAIL}",
                vault_pass.display()
            )
        }),
        (
            "inspection_flags agent_envoy_build_tools_runs_the_embedded_builtin_and_cleans_up",
            plain_vault.clone(),
            format!(
                "model: dryrun:dry-model\n\
                 dry_run: true\n\
                 vault_password_file: '{}'\n\
                 clients:\n\
                 \x20 - type: openai\n\
                 \x20   name: dryrun\n\
                 \x20   auth: none\n\
                 \x20   api_key: unused\n\
                 \x20   models:\n\
                 \x20     - name: dry-model\n\
                 \x20       max_input_tokens: 100000\n\
                 \x20       supports_function_calling: true\n\
                 save: false\n",
                vault_pass.display()
            ),
        ),
        ("inspection_flags EnvoyProbe::new", plain_vault, {
            const DRYRUN_CONFIG_TAIL: &str = r#"clients:
  - type: openai
    name: dryrun
    auth: none
    api_key: unused
    models:
      - name: dry-model
        max_input_tokens: 100000
        supports_function_calling: true
save: false
"#;
            format!(
                "model: dryrun:dry-model\ndry_run: true\nvault_password_file: '{}'\n{DRYRUN_CONFIG_TAIL}",
                vault_pass.display()
            )
        }),
        (
            "inspection_flags sandbox_mutating_vault_flag_stays_strict",
            plain.clone(),
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: unused\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n"
                .to_string(),
        ),
        (
            "inspection_flags probe_strict_mcp_flag",
            plain,
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: unused\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n"
                .to_string(),
        ),
        (
            "mesh_autostart_repl Probe::new",
            DryRunConfig::default()
                .vault_password_file(vault_pass)
                .setting("function_calling_support: true")
                .append(&format!(
                    "save_session: false\n\
                     hooks:\n\
                     \x20 mesh.started:\n\
                     \x20   - name: mark\n\
                     \x20     command: '{started}'\n\
                     \x20 mesh.stopped:\n\
                     \x20   - name: mark\n\
                     \x20     command: '{stopped}'\n\
                     mesh:\n\
                     \x20 enabled: {mesh_enabled}\n\
                     \x20 announce: false\n\
                     \x20 interfaces:\n\
                     {interfaces}"
                ))
                .render(),
            format!(
                "model: dryrun:dry-model\n\
                 dry_run: true\n\
                 vault_password_file: '{vault_pass}'\n\
                 function_calling_support: true\n\
                 clients:\n\
                 \x20 - type: openai\n\
                 \x20   name: dryrun\n\
                 \x20   auth: none\n\
                 \x20   api_key: unused\n\
                 \x20   models:\n\
                 \x20     - name: dry-model\n\
                 \x20       max_input_tokens: 100000\n\
                 \x20       supports_function_calling: true\n\
                 save: false\n\
                 save_session: false\n\
                 hooks:\n\
                 \x20 mesh.started:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{started}'\n\
                 \x20 mesh.stopped:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{stopped}'\n\
                 mesh:\n\
                 \x20 enabled: {mesh_enabled}\n\
                 \x20 announce: false\n\
                 \x20 interfaces:\n\
                 {interfaces}",
                vault_pass = vault_pass.display(),
            ),
        ),
        (
            "mesh_envoy_prune_repl Probe::new",
            DryRunConfig::default()
                .vault_password_file(vault_pass)
                .setting("function_calling_support: true")
                .append(&format!(
                    "save_session: false\n\
                     mesh:\n\
                     \x20 enabled: true\n\
                     \x20 announce: false\n\
                     \x20 interfaces:\n\
                     \x20   - {{type: private, host: 127.0.0.1, port: {relay_port}}}\n\
                     {envoy_block}"
                ))
                .render(),
            format!(
                "model: dryrun:dry-model\n\
                 dry_run: true\n\
                 vault_password_file: '{vault_pass}'\n\
                 function_calling_support: true\n\
                 clients:\n\
                 \x20 - type: openai\n\
                 \x20   name: dryrun\n\
                 \x20   auth: none\n\
                 \x20   api_key: unused\n\
                 \x20   models:\n\
                 \x20     - name: dry-model\n\
                 \x20       max_input_tokens: 100000\n\
                 \x20       supports_function_calling: true\n\
                 save: false\n\
                 save_session: false\n\
                 mesh:\n\
                 \x20 enabled: true\n\
                 \x20 announce: false\n\
                 \x20 interfaces:\n\
                 \x20   - {{type: private, host: 127.0.0.1, port: {relay_port}}}\n\
                 {envoy_block}",
                vault_pass = vault_pass.display(),
            ),
        ),
        (
            "command_prefix_binary write_fixture without a prefix line",
            command_prefix(None),
            command_prefix_old(None),
        ),
        (
            "command_prefix_binary write_fixture with a prefix line",
            command_prefix(Some(prefix)),
            command_prefix_old(Some(prefix)),
        ),
        (
            "macro_bracketing write_fixture_with",
            DryRunConfig::default()
                .vault_password_file(vault_pass)
                .quoted_api_key()
                .append(&format!(
                    "memory: false\n\
                     stream: false\n\
                     hooks:\n\
                     \x20 agent.started:\n\
                     \x20   - name: mark\n\
                     \x20     command: '{started}'\n\
                     \x20 agent.completed:\n\
                     \x20   - name: mark\n\
                     \x20     command: '{completed}'\n\
                     \x20 agent.failed:\n\
                     \x20   - name: mark\n\
                     \x20     command: '{failed}'\n\
                     \x20 agent.interrupted:\n\
                     \x20   - name: mark\n\
                     \x20     command: '{interrupted}'\n\
                     {extra_config}"
                ))
                .render(),
            format!(
                "model: dryrun:dry-model\n\
                 dry_run: true\n\
                 vault_password_file: '{vault_pass}'\n\
                 clients:\n\
                 \x20 - type: openai\n\
                 \x20   name: dryrun\n\
                 \x20   auth: none\n\
                 \x20   api_key: 'unused'\n\
                 \x20   models:\n\
                 \x20     - name: dry-model\n\
                 \x20       max_input_tokens: 100000\n\
                 \x20       supports_function_calling: true\n\
                 save: false\n\
                 memory: false\n\
                 stream: false\n\
                 hooks:\n\
                 \x20 agent.started:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{started}'\n\
                 \x20 agent.completed:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{completed}'\n\
                 \x20 agent.failed:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{failed}'\n\
                 \x20 agent.interrupted:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{interrupted}'\n\
                 {extra_config}",
                vault_pass = vault_pass.display(),
            ),
        ),
    ];

    for (site, rendered, old) in sites {
        assert_eq!(
            rendered, old,
            "{site}: DryRunConfig must render the config.yaml this site wrote before"
        );
    }
}
