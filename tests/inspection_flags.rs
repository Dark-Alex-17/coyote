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
    let tmp_dir = fresh_config_dir(flag.trim_start_matches("--"));

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .arg(flag)
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let leftover: Vec<_> = fs::read_dir(&tmp_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let _ = fs::remove_dir_all(&tmp_dir);

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

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(["--agent", "adversary", "--info"])
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let agents_dir = tmp_dir.join("agents");
    let builtins_installed =
        agents_dir.is_dir() && fs::read_dir(&agents_dir).unwrap().next().is_some();
    let _ = fs::remove_dir_all(&tmp_dir);

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

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .arg("--list-secrets")
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env("IS_SANDBOX", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let leftover: Vec<_> = fs::read_dir(&tmp_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let _ = fs::remove_dir_all(&tmp_dir);

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
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&tmp_dir);

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

// --sync-models downloads and writes models-override.yaml, so it is NOT an
// inspection flag: on a pristine config dir it must take the bootstrap path
// (builtins installed) and, off a terminal, fail on the missing config
// instead of silently operating on defaults. The host path is the one under
// test: a leaked IS_SANDBOX (or provider env) would reroute the missing
// config to the sandbox wizard prompt instead of the fast failure.
#[test]
fn sync_models_stays_on_bootstrap_path() {
    let tmp_dir = fresh_config_dir("sync-models");

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .arg("--sync-models")
        .env("COYOTE_CONFIG_DIR", &tmp_dir)
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let bootstrapped = fs::read_dir(&tmp_dir).unwrap().next().is_some();
    let _ = fs::remove_dir_all(&tmp_dir);

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
