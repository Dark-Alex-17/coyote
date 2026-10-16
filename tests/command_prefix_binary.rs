//! Binary-level contract for the configurable REPL command prefix, driven
//! from a cold config dir the way an operator would set it up: the
//! `command_prefix` key (and its aliases) in config.yaml, the
//! `COYOTE_COMMAND_PREFIX` override, and the effect on a macro run, which
//! is the only non-interactive path that executes REPL lines through the
//! real binary. Every run uses a fresh fake HOME, a dry-run model and a
//! scrubbed sandbox/provider env so host state cannot leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_config_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-command-prefix-{label}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("macros")).unwrap();
    dir
}

/// Lays out a dry-run config dir. `prefix_line` is spliced verbatim into
/// config.yaml so aliases and absent keys can be exercised; the two macros
/// are written in the canonical `.` form (as macro authors are told to)
/// and with a `/`-led first step respectively.
fn write_fixture(dir: &Path, prefix_line: Option<&str>) {
    // Without IS_SANDBOX (scrubbed below) a run insists on a vault password
    // file, so the fixture provides one.
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    let prefix_line = prefix_line.map(|l| format!("{l}\n")).unwrap_or_default();
    // Paths land in single-quoted YAML scalars: double-quoted ones would read
    // the backslashes in a Windows temp path (`C:\Users\...`) as escapes.
    fs::write(
        dir.join("config.yaml"),
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
        ),
    )
    .unwrap();
    fs::write(
        dir.join("macros/dotsteps.yaml"),
        "isolated: false\nsteps:\n  - \".set temperature 0.7\"\n  - \".help\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("macros/slashstep.yaml"),
        "isolated: false\nsteps:\n  - \"/set temperature 0.9\"\n  - \".info\"\n",
    )
    .unwrap();
}

fn run_coyote(dir: &Path, envs: &[(&str, &str)], args: &[&str]) -> Output {
    let home = dir.join("home");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    cmd.args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_COMMAND_PREFIX")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .stdin(Stdio::null());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_ok(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label}: unexpected exit {:?}\nstdout: {}\nstderr: {}",
        output.status,
        stdout_of(output),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The `.info` / `--info` readout's `command_prefix` row value.
fn info_row(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .map(|rest| rest.trim().to_string())
}

fn info_prefix(label: &str, prefix_line: Option<&str>, envs: &[(&str, &str)]) -> String {
    let dir = fresh_config_dir(label);
    let _cleanup = TempDirGuard(dir.clone());
    write_fixture(&dir, prefix_line);
    let output = run_coyote(&dir, envs, &["--info"]);
    assert_ok(label, &output);
    let stdout = stdout_of(&output);
    info_row(&stdout, "command_prefix")
        .unwrap_or_else(|| panic!("{label}: no command_prefix row in --info:\n{stdout}"))
}

#[test]
fn info_reports_the_configured_prefix_and_its_aliases() {
    assert_eq!(info_prefix("absent", None, &[]), "dot");
    assert_eq!(info_prefix("dot", Some("command_prefix: dot"), &[]), "dot");
    assert_eq!(
        info_prefix("slash", Some("command_prefix: slash"), &[]),
        "slash"
    );
    assert_eq!(
        info_prefix("both", Some("command_prefix: both"), &[]),
        "both"
    );
    assert_eq!(
        info_prefix("dot-alias", Some("command_prefix: '.'"), &[]),
        "dot"
    );
    assert_eq!(
        info_prefix("slash-alias", Some("command_prefix: '/'"), &[]),
        "slash"
    );
    assert_eq!(
        info_prefix("either-alias", Some("command_prefix: either"), &[]),
        "both"
    );
}

#[test]
fn env_override_wins_only_when_it_parses() {
    let slash = Some("command_prefix: slash");
    assert_eq!(
        info_prefix("env-both", slash, &[("COYOTE_COMMAND_PREFIX", "both")]),
        "both"
    );
    assert_eq!(
        info_prefix("env-trimmed", slash, &[("COYOTE_COMMAND_PREFIX", " dot ")]),
        "dot"
    );
    assert_eq!(
        info_prefix("env-bogus", slash, &[("COYOTE_COMMAND_PREFIX", "bogus")]),
        "slash"
    );
    assert_eq!(
        info_prefix("env-empty", slash, &[("COYOTE_COMMAND_PREFIX", "")]),
        "slash"
    );
}

#[test]
fn bogus_config_value_is_refused_naming_the_variants() {
    let dir = fresh_config_dir("bogus-config");
    let _cleanup = TempDirGuard(dir.clone());
    write_fixture(&dir, Some("command_prefix: bogus"));
    let output = run_coyote(&dir, &[], &["--info"]);
    assert!(
        !output.status.success(),
        "a bogus command_prefix must fail config load\nstdout: {}",
        stdout_of(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("command_prefix")
            && stderr.contains("`dot`")
            && stderr.contains("`slash`")
            && stderr.contains("`both`"),
        "stderr must name the key and the accepted variants:\n{stderr}"
    );
}

/// Macro steps are authored in `.` form; the help they print renders in
/// the OPERATOR's sigil, and the `.set` step takes effect under every
/// setting.
#[test]
fn macro_dot_steps_run_under_slash_and_help_renders_with_the_configured_sigil() {
    for (label, prefix_line, sigil) in [
        ("macro-dot", None, '.'),
        ("macro-slash", Some("command_prefix: slash"), '/'),
        ("macro-both", Some("command_prefix: both"), '.'),
    ] {
        let dir = fresh_config_dir(label);
        let _cleanup = TempDirGuard(dir.clone());
        write_fixture(&dir, prefix_line);
        let output = run_coyote(&dir, &[], &["--macro", "dotsteps"]);
        assert_ok(label, &output);
        let stdout = stdout_of(&output);
        let other = if sigil == '/' { '.' } else { '/' };
        for name in ["help", "info", "set", "exit"] {
            assert!(
                stdout
                    .lines()
                    .any(|l| l.starts_with(&format!("{sigil}{name} "))),
                "{label}: help must list {sigil}{name}:\n{stdout}"
            );
            assert!(
                !stdout
                    .lines()
                    .any(|l| l.starts_with(&format!("{other}{name} "))),
                "{label}: help must not list {other}{name}:\n{stdout}"
            );
        }
        assert!(
            stdout.contains(&format!(r#"Type "{sigil}help""#))
                || stdout.contains(&format!("\"{sigil}redraw\"")),
            "{label}: help prose must use the configured sigil:\n{stdout}"
        );
    }
}

/// A `/`-led macro step is ordinary chat for a dot operator (the dry-run
/// model echoes it and nothing is set) and a command for slash and both.
#[test]
fn macro_slash_step_is_chat_under_dot_and_a_command_otherwise() {
    for (label, prefix_line, expected_temperature) in [
        ("slashstep-dot", None, "null"),
        ("slashstep-slash", Some("command_prefix: slash"), "0.9"),
        ("slashstep-both", Some("command_prefix: both"), "0.9"),
    ] {
        let dir = fresh_config_dir(label);
        let _cleanup = TempDirGuard(dir.clone());
        write_fixture(&dir, prefix_line);
        let output = run_coyote(&dir, &[], &["--macro", "slashstep"]);
        assert_ok(label, &output);
        let stdout = stdout_of(&output);
        assert_eq!(
            info_row(&stdout, "temperature").as_deref(),
            Some(expected_temperature),
            "{label}:\n{stdout}"
        );
        assert!(
            !stdout.contains("Unknown command"),
            "{label}: a slash-led line is never an unknown command:\n{stdout}"
        );
    }
}
