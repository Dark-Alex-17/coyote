//! Static pins for the mesh relay setup scripts (`scripts/mesh-relay.sh`,
//! `scripts/mesh-relay.ps1`) and the installer hooks that hand off to them.
//!
//! The scripts write a Reticulum config once and never overwrite it, so a wrong
//! first write is permanent for that host; these tests hold the tokens that make
//! the written config and the service environment work: the rns version the
//! interop harness is verified against (read from the propagation-node
//! Dockerfile rather than duplicated here), the `[logging]` section without
//! which `rnsd -v` is ignored, the unbuffered-stdout environment every service
//! log sink needs, and the shell/PowerShell strict-mode scaffolding.

use std::fs;
use std::path::{Path, PathBuf};

use fancy_regex::Regex;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scripts_dir() -> PathBuf {
    repo_root().join("scripts")
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const RELAY_SCRIPTS: [&str; 2] = ["mesh-relay.sh", "mesh-relay.ps1"];
const INSTALLERS: [&str; 2] = ["install_coyote.sh", "install_coyote.ps1"];

/// The value of `ARG RNS_VERSION=` in the propagation-node Dockerfile, the
/// single place the interop-verified rns version is declared.
fn dockerfile_rns_version() -> String {
    let path = repo_root()
        .join("deployment")
        .join("propagation-node")
        .join("Dockerfile");
    let dockerfile = read(&path);
    dockerfile
        .lines()
        .find_map(|line| line.strip_prefix("ARG RNS_VERSION="))
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| panic!("{} has no `ARG RNS_VERSION=` line", path.display()))
}

#[test]
fn relay_scripts_default_to_the_dockerfile_rns_version() {
    let version = dockerfile_rns_version();
    let spec = format!("rns=={version}");
    let version_literal = Regex::new(r"\d+\.\d+\.\d+").unwrap();
    for name in RELAY_SCRIPTS {
        let script = read(scripts_dir().join(name));
        assert!(
            script.contains(&spec),
            "{name} must install `{spec}`: deployment/propagation-node/Dockerfile says `ARG RNS_VERSION={version}` and that is the version the interop harness verified; bump both together"
        );
        for (line_no, line) in script.lines().enumerate() {
            for found in version_literal.find_iter(line) {
                let found = found.unwrap().as_str();
                let is_the_spec = line.contains(&spec) && found == version;
                let is_loopback = found == "127.0.0" && line.contains("127.0.0.1");
                assert!(
                    is_the_spec || is_loopback,
                    "{name}:{} carries the version literal {found:?} outside the `{spec}` constant; a bump would leave it stale: {line}",
                    line_no + 1
                );
            }
        }
    }
}

/// The Scheduled Task half of the Windows script: a logon-triggered, interactive,
/// hidden, never-time-limited task that runs the `rnsd.cmd` shim, looked up before it
/// is registered and started only when it is not already running. No CI runner has
/// an interactive logon, so these tokens are the standing evidence for that contract.
#[test]
fn the_windows_script_carries_the_scheduled_task_and_readiness_contract() {
    let ps1 = read(scripts_dir().join("mesh-relay.ps1"));
    for (token, why) in [
        (
            "New-ScheduledTaskTrigger -AtLogOn",
            "the task must start at logon",
        ),
        (
            "-LogonType Interactive",
            "an interactive logon type keeps the task in the user's session",
        ),
        ("-Hidden", "the task window must stay hidden"),
        (
            "ExecutionTimeLimit",
            "a daemon must never be killed by the default 72 h time limit",
        ),
        (
            "Get-ScheduledTask",
            "the task must be looked up before it is registered",
        ),
        ("Register-ScheduledTask", "the task must be registered"),
        (
            "Start-ScheduledTask",
            "the task must be started when it is not running",
        ),
        ("'Running'", "an already running task must not be restarted"),
        (
            "rnsd.cmd",
            "the shim is a .cmd because symlinks need developer mode",
        ),
        (
            "System.Net.Sockets.TcpClient",
            "the readiness probe must be the TcpClient, not Test-NetConnection",
        ),
        (
            "$env:LOCALAPPDATA",
            "the task log must live under %LOCALAPPDATA%",
        ),
        (
            "$ErrorActionPreference = 'Stop'",
            "every native failure must stop the script",
        ),
        ("Set-StrictMode -Version Latest", "strict mode"),
    ] {
        assert!(ps1.contains(token), "mesh-relay.ps1 lacks {token:?}: {why}");
    }
    assert!(
        !ps1.contains("Test-NetConnection"),
        "Test-NetConnection takes seconds per probe and is absent from pwsh on other OSes"
    );
    for pattern in [" ?? ", "??="] {
        assert!(
            !ps1.contains(pattern),
            "mesh-relay.ps1 uses {pattern:?}, which Windows PowerShell 5.1 cannot parse"
        );
    }
    let installer = read(scripts_dir().join("install_coyote.ps1"));
    for (token, why) in [
        (
            "[Environment]::UserInteractive",
            "the prompt needs an interactive session",
        ),
        (
            "-not [Console]::IsInputRedirected -and -not [Console]::IsOutputRedirected",
            "a piped installer must never read a script line as its answer (the bash twin tests -t 0 and -t 1)",
        ),
        ("-not $env:CI", "CI runners are never prompted"),
    ] {
        assert!(
            installer.contains(token),
            "install_coyote.ps1 mesh prompt gate lacks {token:?}: {why}"
        );
    }
    assert!(
        installer.contains("https://raw.githubusercontent.com/$Repo/main/scripts"),
        "a piped installer must fetch the relay script from the README's ref"
    );
}

/// macOS ships bash 3.2 and the README one-liners land there; the two bash scripts
/// must avoid every construct bash 4 added, and the `$(cat <<EOF ...)` shape whose
/// heredoc bash 3.2 mis-scans when the prose holds an apostrophe.
#[test]
fn the_bash_scripts_stay_within_bash_3_2() {
    for name in ["mesh-relay.sh", "install_coyote.sh"] {
        let script = read(scripts_dir().join(name));
        for (line_no, line) in script.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            for (needle, construct) in [
                ("mapfile", "mapfile (bash 4.0)"),
                ("readarray", "readarray (bash 4.0)"),
                ("declare -A", "associative arrays (bash 4.0)"),
                ("|&", "|& pipe (bash 4.0)"),
                (";;&", ";;& case fall-through (bash 4.0)"),
                ("[[ -v", "[[ -v (bash 4.2)"),
                ("&>>", "&>> redirection (bash 4.0)"),
                (
                    "$(cat <<",
                    "$(cat <<EOF ...) with a heredoc (bash 3.2 mis-scans apostrophes inside it)",
                ),
            ] {
                assert!(
                    !line.contains(needle),
                    "{name}:{} uses {construct}: {line}",
                    line_no + 1
                );
            }
            for case_mod in [",,}", "^^}", ",}", "^}"] {
                let uses_case_mod = line.match_indices("${").any(|(start, _)| {
                    line[start..].contains(case_mod)
                        && !line[start..line[start..].find(case_mod).unwrap() + start].contains('}')
                });
                assert!(
                    !uses_case_mod,
                    "{name}:{} uses ${{var{case_mod} case modification (bash 4.0): {line}",
                    line_no + 1
                );
            }
        }
        assert!(
            script.contains("set -euo pipefail"),
            "{name} must fail fast under set -euo pipefail"
        );
        assert!(
            !script.contains("sudo "),
            "{name} must never escalate: everything it writes belongs to the user"
        );
    }
}

#[test]
fn relay_scripts_write_the_logging_section_and_unbuffered_service_environment() {
    for name in RELAY_SCRIPTS {
        let script = read(scripts_dir().join(name));
        assert!(
            script.contains("[logging]"),
            "{name} must write a `[logging]` section: rnsd applies -v flags only when it exists"
        );
        assert!(
            script.contains("loglevel = 4"),
            "{name} must write `loglevel = 4` (LOG_INFO) so TCP interface lines are visible with -vv"
        );
        assert!(
            script.contains("PYTHONUNBUFFERED=1"),
            "{name} must set PYTHONUNBUFFERED=1 in the service environment: rnsd print()s to a non-TTY sink and the lines sit in the block buffer otherwise"
        );
        assert!(
            script.contains("ingress_control = No"),
            "{name} must write `ingress_control = No` on the loopback listener: the default holds a brand-new identity's first announce"
        );
        assert!(
            !script.contains("share_instance = No"),
            "{name} must not write `share_instance = No`: rnstatus, rnpath, Sideband and NomadNet on the host attach to the shared instance"
        );
        assert!(
            script.contains("exit 3"),
            "{name} must exit 3 when the listener does not come up: the README documents that code"
        );
    }
    let sh = read(scripts_dir().join("mesh-relay.sh"));
    assert!(
        sh.contains("<key>PYTHONUNBUFFERED</key>"),
        "mesh-relay.sh must set PYTHONUNBUFFERED in the launchd plist's EnvironmentVariables, not only the systemd unit"
    );
    assert!(
        sh.contains("READY_TIMEOUT=30"),
        "mesh-relay.sh must wait 30 s for the listener: the documented exit 3 contract"
    );
    let ps1 = read(scripts_dir().join("mesh-relay.ps1"));
    assert!(
        ps1.contains("$ReadyTimeoutSeconds = 30"),
        "mesh-relay.ps1 must wait 30 s for the listener: the documented exit 3 contract"
    );
}

#[test]
fn installers_offer_the_mesh_relay_hand_off() {
    let sh = read(scripts_dir().join("install_coyote.sh"));
    assert!(
        sh.contains("--with-mesh"),
        "install_coyote.sh must accept `--with-mesh` so the docs can cite it as the one-command mesh setup"
    );
    let ps1 = read(scripts_dir().join("install_coyote.ps1"));
    assert!(
        ps1.contains("-WithMesh"),
        "install_coyote.ps1 must accept `-WithMesh` so the docs can cite it as the one-command mesh setup"
    );
}

#[test]
fn relay_scripts_carry_the_strict_mode_scaffolding() {
    let sh = read(scripts_dir().join("mesh-relay.sh"));
    assert_eq!(
        sh.lines().next(),
        Some("#!/usr/bin/env bash"),
        "mesh-relay.sh must start with the env shebang: macOS /bin/bash is 3.2 and users may have a newer bash first on PATH"
    );
    assert!(
        sh.contains("set -euo pipefail"),
        "mesh-relay.sh must fail fast: a half-run install must not continue into the config write"
    );
    assert_eq!(
        sh.lines().rev().find(|line| !line.trim().is_empty()),
        Some("main \"$@\""),
        "mesh-relay.sh must end with `main \"$@\"` so a truncated download executes nothing"
    );
    let ps1 = read(scripts_dir().join("mesh-relay.ps1"));
    assert!(
        ps1.contains("Set-StrictMode -Version Latest"),
        "mesh-relay.ps1 must enable strict mode so an unset variable fails instead of writing an empty path"
    );
}

#[test]
fn scripts_are_ascii_lf_and_free_of_plan_labels() {
    let tracking_id = Regex::new(r"(?i)\b(task|plan|scope)-[0-9A-Z]").unwrap();
    for name in RELAY_SCRIPTS.iter().chain(INSTALLERS.iter()) {
        let path = scripts_dir().join(name);
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            bytes.is_ascii(),
            "{name} must be plain ASCII so PowerShell 5.1 reads it without a BOM and curl | bash never meets an encoding surprise"
        );
        assert!(
            !bytes.contains(&b'\r'),
            "{name} must use LF line endings: bash rejects CRLF scripts with `\\r: command not found`"
        );
        let text = String::from_utf8(bytes).unwrap();
        for (line_no, line) in text.lines().enumerate() {
            assert!(
                !tracking_id.is_match(line).unwrap(),
                "{name}:{} references plan/task tracking; that belongs in commit messages: {line}",
                line_no + 1
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(scripts_dir().join("mesh-relay.sh")).unwrap();
        assert!(
            metadata.permissions().mode() & 0o111 != 0,
            "mesh-relay.sh should be executable in the tree so `./scripts/mesh-relay.sh` works from a checkout"
        );
    }
}
