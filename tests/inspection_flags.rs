//! Binary-level contract for inspection flags on a pristine config dir:
//! exit 0 and NOTHING written into the config dir (no first-run wizard,
//! no builtins bootstrap). Only --info guarantees output on stdout; the
//! list flags legitimately emit an empty listing on a pristine dir, so
//! their contract is exit 0 + zero writes. Vault-backed flags like
//! --list-secrets depend on host state and are covered by
//! scripts/usage-probe-inspection.sh instead. Agent runs, by contrast,
//! must still bootstrap builtins even when combined with --info — see
//! agent_info_on_empty_config_dir_bootstraps_builtins. Flags that write,
//! like --sync-models, are NOT inspection flags and must stay on the
//! bootstrap path — see sync_models_stays_on_bootstrap_path.
//!
//! Every test runs against a fresh fake HOME and (unless sandbox mode is the
//! subject under test) a scrubbed IS_SANDBOX/provider env, so host state —
//! a real ~/.coyote_password or a sandbox shell — cannot mask
//! virgin-environment regressions.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn fresh_config_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp_dir = env::temp_dir().join(format!("coyote-inspection-{label}-{unique}"));
    fs::create_dir_all(&tmp_dir).unwrap();
    tmp_dir
}

fn probe_inspection_flag(flag: &str, require_stdout: bool) {
    let label = flag.trim_start_matches("--");
    let tmp_dir = fresh_config_dir(label);
    let home_dir = fresh_config_dir(&format!("{label}-home"));

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .arg(flag)
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        // A fresh fake HOME hides any real ~/.coyote_password on the host;
        // USERPROFILE is the best-effort Windows equivalent (dirs resolves
        // home via the Known Folder API there). HOME is set, never removed:
        // dirs::home_dir() panics inside gman when home can't be determined.
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        // A leaked sandbox/provider env would reroute the vault and config
        // paths under test.
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        // Workspace mcp.json discovery resolves from the CWD, so pin it to
        // the isolated fake HOME to keep host state out of the probe.
        .current_dir(&home_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let leftover: Vec<_> = fs::read_dir(&tmp_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        !home_dir.join(".coyote_password").exists(),
        "{flag}: the lenient inspection path must never bootstrap a password file"
    );
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&home_dir);

    assert!(
        output.status.success(),
        "{flag}: expected exit 0 on an empty config dir, got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if require_stdout {
        assert!(
            !output.stdout.is_empty(),
            "{flag}: expected some output on stdout"
        );
    }
    assert!(
        leftover.is_empty(),
        "{flag}: inspection flags must write nothing into the config dir: {leftover:?}"
    );
}

#[test]
fn info_on_empty_config_dir_writes_nothing() {
    probe_inspection_flag("--info", true);
}

// List flags on a pristine dir correctly print an empty listing, so the
// contract is exit 0 + zero writes only (no stdout assertion).

#[test]
fn list_models_on_empty_config_dir_writes_nothing() {
    probe_inspection_flag("--list-models", false);
}

#[test]
fn list_agents_on_empty_config_dir_writes_nothing() {
    probe_inspection_flag("--list-agents", false);
}

#[test]
fn mcp_list_on_empty_config_dir_writes_nothing() {
    probe_inspection_flag("--mcp-list", false);
}

// The wizard bypass must not swallow the builtins bootstrap for agent runs:
// `--agent <builtin> --info` on a totally fresh config dir has to install
// the bundled definitions before it can render the agent readout.
#[test]
fn agent_info_on_empty_config_dir_bootstraps_builtins() {
    let tmp_dir = fresh_config_dir("agent-info");
    let home_dir = fresh_config_dir("agent-info-home");

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(["--agent", "adversary", "--info"])
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .current_dir(&home_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let agents_dir = tmp_dir.join("agents");
    let builtins_installed =
        agents_dir.is_dir() && fs::read_dir(&agents_dir).unwrap().next().is_some();
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&home_dir);

    assert!(
        output.status.success(),
        "--agent adversary --info: expected exit 0 on an empty config dir, got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        builtins_installed,
        "--agent adversary --info: expected builtin agents installed into {}",
        agents_dir.display()
    );
    // The --info readout echoes the loaded agent definition including its
    // global_tools, so a builtin tool name there proves the definitions were
    // loaded into the agent's tool scope, not just written to disk.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fs_cat.sh"),
        "--agent adversary --info: expected builtin tool fs_cat.sh in the readout\nstdout: {stdout}"
    );
}

// Sandbox-mode vault contract: `--list-secrets` is an inspection readout and
// must exit 0 with an informational message (and zero writes) when the vault
// is unreachable inside a sandbox, while mutating vault flags stay strict.
#[test]
fn sandbox_list_secrets_prints_informational_message_and_writes_nothing() {
    let tmp_dir = fresh_config_dir("sandbox-list-secrets");
    let home_dir = fresh_config_dir("sandbox-list-secrets-home");

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .arg("--list-secrets")
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env("IS_SANDBOX", "1")
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .current_dir(&home_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let leftover: Vec<_> = fs::read_dir(&tmp_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&home_dir);

    assert!(
        output.status.success(),
        "--list-secrets in sandbox mode: expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("The vault is unavailable in sandbox mode"),
        "--list-secrets in sandbox mode: expected the informational readout, got: {combined}"
    );
    assert!(
        leftover.is_empty(),
        "--list-secrets in sandbox mode: expected zero writes into the config dir: {leftover:?}"
    );
}

// Mutating vault flags are NOT inspection flags: in sandbox mode they must
// fail hard with the vault-management error, never the soft informational
// message. A minimal valid config is provided so the run gets past model
// resolution and reaches the vault gate.
#[test]
fn sandbox_mutating_vault_flag_stays_strict() {
    let tmp_dir = fresh_config_dir("sandbox-delete-secret");
    let home_dir = fresh_config_dir("sandbox-delete-secret-home");
    fs::write(
        tmp_dir.join("config.yaml"),
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
         save: false\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(["--delete-secret", "probe"])
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env("IS_SANDBOX", "1")
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .current_dir(&home_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&home_dir);

    assert!(
        !output.status.success(),
        "--delete-secret in sandbox mode: expected a non-zero exit, got {:?}\nstdout: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Vault management is disabled in sandbox mode"),
        "--delete-secret in sandbox mode: expected the strict vault error, got: {combined}"
    );
}

// The --mcp-list leniency must not leak into the mutating MCP flags on the
// host path: with no password file (fresh fake HOME) and no sandbox env,
// --mcp-get/--mcp-remove/--mcp-add have to fail on the strict vault
// requirement rather than operate with an unconfigured vault.
fn probe_strict_mcp_flag(label: &str, args: &[&str]) {
    let tmp_dir = fresh_config_dir(&format!("{label}-no-vault"));
    let home_dir = fresh_config_dir(&format!("{label}-no-vault-home"));
    fs::write(
        tmp_dir.join("config.yaml"),
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
         save: false\n",
    )
    .unwrap();
    fs::write(
        tmp_dir.join("mcp.json"),
        r#"{"mcpServers":{"probe-server":{"type":"stdio","command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .current_dir(&home_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // The strict path must not touch the server registry either.
    let mcp_json_after = fs::read_to_string(tmp_dir.join("mcp.json")).unwrap();
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&home_dir);

    assert!(
        !output.status.success(),
        "{label} without a vault: expected a non-zero exit, got {:?}\nstdout: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("A password file is required"),
        "{label} without a vault: expected the strict vault error, got: {combined}"
    );
    assert!(
        mcp_json_after.contains("probe-server"),
        "{label} without a vault: mcp.json must be left untouched, got: {mcp_json_after}"
    );
}

#[test]
fn mcp_mutating_flag_stays_strict_without_vault() {
    probe_strict_mcp_flag("mcp-remove", &["--mcp-remove", "probe-server"]);
}

// --mcp-get is read-only but consumes resolved server config (which may
// embed secrets), so it stays on the strict vault path by design.
#[test]
fn mcp_get_stays_strict_without_vault() {
    probe_strict_mcp_flag("mcp-get", &["--mcp-get", "probe-server"]);
}

#[test]
fn mcp_add_stays_strict_without_vault() {
    probe_strict_mcp_flag("mcp-add", &["--mcp-add", "newsrv", "--", "echo", "hi"]);
}

// The listing leniency exercised with content: a configured, secret-free
// server must be listed even when no vault exists (no password file, no
// sandbox env), and the readout must not write anything new into the
// config dir.
#[test]
fn mcp_list_lists_configured_server_without_vault() {
    let tmp_dir = fresh_config_dir("mcp-list-configured");
    let home_dir = fresh_config_dir("mcp-list-configured-home");
    fs::write(
        tmp_dir.join("mcp.json"),
        r#"{"mcpServers":{"probe-server":{"type":"stdio","command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .arg("--mcp-list")
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .current_dir(&home_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let leftover: Vec<_> = fs::read_dir(&tmp_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&home_dir);

    assert!(
        output.status.success(),
        "--mcp-list with a configured server and no vault: expected exit 0, got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("probe-server"),
        "--mcp-list with a configured server: expected 'probe-server' in stdout, got: {stdout}"
    );
    assert_eq!(
        leftover,
        vec![std::ffi::OsString::from("mcp.json")],
        "--mcp-list must not write into the config dir beyond the pre-seeded mcp.json"
    );
}

// --sync-models downloads and writes models-override.yaml, so it is NOT an
// inspection flag: on a pristine config dir it must take the bootstrap path
// (builtins installed) and, off a terminal, fail on the missing config
// instead of silently operating on defaults. The host path is the one under
// test: a leaked IS_SANDBOX (or provider env) would reroute the missing
// config to the sandbox wizard prompt instead of the fast failure.
#[test]
fn sync_models_stays_on_bootstrap_path() {
    let tmp_dir = fresh_config_dir("sync-models");
    let home_dir = fresh_config_dir("sync-models-home");

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .arg("--sync-models")
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .current_dir(&home_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let bootstrapped = fs::read_dir(&tmp_dir).unwrap().next().is_some();
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::remove_dir_all(&home_dir);

    assert!(
        !output.status.success(),
        "--sync-models: expected the strict missing-config failure on an empty config dir, got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        bootstrapped,
        "--sync-models: expected the builtins bootstrap to populate the config dir"
    );
}
