//! Static checks for the mesh relay setup scripts (`scripts/mesh-relay.sh`,
//! `scripts/mesh-relay.ps1`), the installer hooks that hand off to them, and the
//! Docker image's rnsd bundle (`Dockerfile`, `scripts/docker-entrypoint.sh`,
//! `scripts/reticulum.config.tmpl`, `scripts/image-smoke.sh`), plus the third-party
//! license texts NOTICE points at and the documents that name the mesh transport
//! dependency, which must agree with `Cargo.toml`.
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
const IMAGE_SCRIPTS: [&str; 3] = [
    "docker-entrypoint.sh",
    "image-smoke.sh",
    "reticulum.config.tmpl",
];

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
/// an interactive logon, so these tokens are the only check that runs against the real cmdlets.
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
            "$existing.Actions",
            "a registered task's definition is compared with the one this run would write",
        ),
        (
            "    Set-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null",
            "a changed definition is stored in full with an executed Set-ScheduledTask, not only described",
        ),
        (
            "function Get-CurrentUserName",
            "the WindowsIdentity lookup sits behind a function so the task harness can stand in for it off-Windows",
        ),
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
        ps1.matches("Note: the service definition changed; apply it with:")
            .count()
            == 2,
        "mesh-relay.ps1 must print the changed-definition Note on both the dry run and the real run"
    );
    let shape = ps1
        .split("function Get-TaskShape")
        .nth(1)
        .and_then(|rest| rest.split("\nfunction ").next())
        .expect("mesh-relay.ps1 defines Get-TaskShape");
    for token in ["CimClassName", "LogonType", "Hidden", "ExecutionTimeLimit"] {
        assert!(
            shape.contains(token),
            "Get-TaskShape must compare {token}: a registered task is matched on its whole shape, not only the action arguments"
        );
    }
    let stop_lines: Vec<&str> = ps1
        .lines()
        .filter(|line| line.contains("Stop-ScheduledTask"))
        .collect();
    assert!(
        !stop_lines.is_empty()
            && stop_lines
                .iter()
                .all(|line| line.contains("Note:") || line.contains("Would run:")),
        "Stop-ScheduledTask is advice only; a running task is never restarted by the script: {stop_lines:?}"
    );
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
        installer.contains("https://raw.githubusercontent.com/$Repo/refs/heads/main/scripts"),
        "a piped installer must fetch the relay script from the README's ref"
    );
    // Only an explicit -WithMesh failure is exit 3; the prompt path records its failure
    // in $script:MeshRc too, so the exit must key on a value set in the -WithMesh branch alone.
    assert_eq!(
        installer.matches("exit 3").count(),
        1,
        "install_coyote.ps1 has one exit 3"
    );
    assert!(
        installer.contains("if ($script:MeshExplicitRc -ne 0) { exit 3 }"),
        "exit 3 must key on the explicit-flag result, not the shared $script:MeshRc"
    );
    assert_eq!(
        installer
            .matches("$script:MeshExplicitRc = $script:MeshRc")
            .count(),
        1,
        "the explicit-flag result is captured once, inside the -WithMesh branch"
    );
}

/// The install ladder of the Windows script: present, then uv, then pipx, then `py -3`,
/// then `python`, each python probed against the 3.9 floor, exit 2 when every rung is
/// missing; and the venv lands under COYOTE_CONFIG_DIR, else XDG_CONFIG_HOME, else
/// %APPDATA%\coyote. CI strips uv and pipx to reach `py -3`; the `python` rung and the
/// exit 2 behind it are held here by position.
#[test]
fn the_windows_script_tries_py_3_before_python_enforces_the_3_9_floor_and_falls_back_to_appdata() {
    let ps1 = read(scripts_dir().join("mesh-relay.ps1"));
    let index_of = |token: &str, why: &str| {
        ps1.find(token)
            .unwrap_or_else(|| panic!("mesh-relay.ps1 lacks {token:?}: {why}"))
    };

    let uv = index_of(
        "Get-Command uv -CommandType Application",
        "uv is the first install rung after an existing rnsd",
    );
    let pipx = index_of(
        "Get-Command pipx -CommandType Application",
        "pipx is the rung after uv",
    );
    let py = index_of(
        "Test-PythonOk -File 'py' -Prefix @('-3')",
        "the py launcher is asked for a Python 3 explicitly",
    );
    let python = index_of(
        "Test-PythonOk -File 'python' -Prefix @()",
        "a bare python on PATH is the last rung",
    );
    assert!(
        uv < pipx,
        "uv must be probed before pipx: the ladder is uv → pipx → py -3 → python"
    );
    assert!(
        pipx < py,
        "pipx must be probed before `py -3`: a venv is the rung of last resort"
    );
    assert!(
        py < python,
        "`py -3` must be tried before `python`: on Windows `python` may be the Store alias or a Python 2"
    );

    assert!(
        ps1.contains("sys.exit(0 if sys.version_info >= (3, 9) else 1)"),
        "mesh-relay.ps1 must probe each python against the 3.9 floor before building the venv"
    );
    let failure = index_of(
        "no way to install rnsd",
        "every missing rung must be reported as one failure",
    );
    let next_exit = ps1[failure..]
        .lines()
        .map(str::trim_start)
        .find(|line| line.starts_with("exit "))
        .expect("mesh-relay.ps1 exits after the `no way to install rnsd` failure");
    assert_eq!(
        next_exit, "exit 2",
        "the `no way to install rnsd` failure must be exit 2, the documented missing-prerequisite code"
    );

    let coyote_config_dir = index_of(
        "$env:COYOTE_CONFIG_DIR",
        "an explicit COYOTE_CONFIG_DIR must win",
    );
    let xdg = index_of(
        "Join-Path $env:XDG_CONFIG_HOME 'coyote'",
        "XDG_CONFIG_HOME is honoured when set, as the bash twin does",
    );
    let appdata = index_of(
        "Join-Path $env:APPDATA 'coyote'",
        "with neither variable the config dir is %APPDATA%\\coyote",
    );
    let venv = index_of(
        "Join-Path $coyoteConfig 'mesh\\rns-venv'",
        "the venv lives under the resolved config dir",
    );
    assert!(
        coyote_config_dir < xdg && xdg < appdata,
        "the config dir precedence must be COYOTE_CONFIG_DIR → XDG_CONFIG_HOME → APPDATA"
    );
    assert!(
        appdata < venv,
        "the venv path must be derived from the config dir after it is resolved"
    );
}

/// launchd has no `daemon-reload`: a loaded agent only picks up a rewritten plist after
/// `bootout` + `bootstrap`, so the script says so in its Note and prints that pair,
/// never running it against a loaded agent.
#[test]
fn the_bash_script_prints_the_launchd_reload_pair_and_never_runs_it() {
    let sh = read(scripts_dir().join("mesh-relay.sh"));
    for needle in [
        "Note: the service definition changed and the plist was rewritten; launchd cannot reload a loaded agent in place, so apply it with: launchctl bootout",
        "Note: the service definition changed and the plist would be rewritten; launchd cannot reload a loaded agent in place, so apply it with: launchctl bootout",
    ] {
        assert!(
            sh.contains(needle),
            "mesh-relay.sh macOS Note must state why the pair is needed: {needle:?}"
        );
    }
    let bootout_lines: Vec<&str> = sh
        .lines()
        .filter(|line| line.contains("launchctl bootout"))
        .collect();
    assert!(
        !bootout_lines.is_empty()
            && bootout_lines
                .iter()
                .all(|line| line.trim_start().starts_with("log \"Note:")),
        "launchctl bootout is advice only; a loaded agent is never torn down by the script: {bootout_lines:?}"
    );
}

/// Both scripts write the config where rnsd itself looks, in Reticulum's order, pass
/// that directory to the daemon with `--config` in every service definition, and
/// upgrade an older rnsd through its installer unless told to keep it.
#[test]
fn relay_scripts_target_the_config_dir_rnsd_reads_and_compare_the_version() {
    let sh = read(scripts_dir().join("mesh-relay.sh"));
    let ps1 = read(scripts_dir().join("mesh-relay.ps1"));

    let unit = sh
        .split("systemd_unit_text() {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("mesh-relay.sh defines systemd_unit_text");
    assert!(
        unit.contains("ExecStart=\"$RNSD\" --config \"$RNS_DIR\""),
        "the systemd unit must pass the chosen config dir, or the daemon may read another file than the script printed"
    );
    let plist = sh
        .split("launchd_plist_text() {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("mesh-relay.sh defines launchd_plist_text");
    assert!(
        plist.contains("<string>--config</string>"),
        "the launchd ProgramArguments must pass --config"
    );
    let action = ps1
        .split("function Get-TaskActionArgument {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("mesh-relay.ps1 defines Get-TaskActionArgument");
    assert!(
        action.contains("--config `\"$script:RnsDir`\""),
        "the Scheduled Task action must pass the chosen config dir"
    );

    for (name, script, keep, xdg) in [
        (
            "mesh-relay.sh",
            &sh,
            "--keep-rnsd",
            "$HOME/.config/reticulum",
        ),
        ("mesh-relay.ps1", &ps1, "-KeepRnsd", "'.config\\reticulum'"),
    ] {
        assert!(
            script.contains(keep),
            "{name} lacks the {keep} flag that keeps an older rnsd"
        );
        assert!(
            script.contains("--force"),
            "{name} must upgrade an older rnsd with the installer's --force"
        );
        assert!(
            script.contains(xdg),
            "{name} must consider {xdg}, which rnsd reads before ~/.reticulum when it holds a config"
        );
    }
    assert!(
        sh.lines().any(|line| {
            line.trim_start().starts_with('#') && line.contains("COYOTE_MESH_ETC_RETICULUM")
        }),
        "mesh-relay.sh must document the COYOTE_MESH_ETC_RETICULUM override in a comment"
    );
    assert!(
        sh.contains("/etc/reticulum"),
        "mesh-relay.sh must check /etc/reticulum first, as Reticulum does"
    );
}

/// macOS ships bash 3.2 and the README one-liners land there; the two bash scripts
/// must avoid every construct bash 4 added, and the `$(cat <<EOF ...)` shape whose
/// heredoc bash 3.2 mis-scans when the prose holds an apostrophe.
#[test]
fn the_bash_scripts_stay_within_bash_3_2() {
    let quoted_replacement = Regex::new(r#"\$\{[^}]*//[^}]*/""#).unwrap();
    for name in ["mesh-relay.sh", "install_coyote.sh"] {
        let script = read(scripts_dir().join(name));
        for (line_no, line) in script.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            for (needle, construct, haystack) in [
                ("mapfile", "mapfile (bash 4.0)"),
                ("readarray", "readarray (bash 4.0)"),
                ("declare -A", "associative arrays (bash 4.0)"),
                ("|&", "|& pipe (bash 4.0)"),
                (";;&", ";;& case fall-through (bash 4.0)"),
                ("[[ -v", "[[ -v (bash 4.2)"),
                ("&>>", "&>> redirection (bash 4.0)"),
                ("local -n", "local -n namerefs (bash 4.3)"),
                ("declare -n", "declare -n namerefs (bash 4.3)"),
                ("coproc", "coproc (bash 4.0)"),
                ("EPOCHSECONDS", "EPOCHSECONDS (bash 5.0)"),
                ("EPOCHREALTIME", "EPOCHREALTIME (bash 5.0)"),
                ("@Q}", "${var@Q} parameter transformation (bash 4.4)"),
                (";&", ";& case fall-through (bash 4.0)"),
                (
                    "$(cat <<",
                    "$(cat <<EOF ...) with a heredoc (bash 3.2 mis-scans apostrophes inside it)",
                ),
            ]
            .map(|(needle, construct)| {
                // `;;&` is reported on its own; the `;&` needle must not fire on it too.
                let haystack = if needle == ";&" {
                    line.replace(";;&", "")
                } else {
                    line.to_string()
                };
                (needle, construct, haystack)
            }) {
                assert!(
                    !haystack.contains(needle),
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
            assert!(
                !quoted_replacement.is_match(line).unwrap(),
                "{name}:{} quotes the replacement of a pattern substitution; bash 3.2 keeps those quotes literally in the result: {line}",
                line_no + 1
            );
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

fn workflow(file: &str) -> String {
    read(repo_root().join(".github").join("workflows").join(file)).replace("\r\n", "\n")
}

/// The text of the job `name` in the workflow `file`: from its header to the next
/// 2-space-indented key (the next job). Jobs are the only keys at that indent under `jobs:`.
fn workflow_job(file: &str, name: &str) -> String {
    let workflow = workflow(file);
    let header = format!("\n  {name}:\n");
    let start = workflow
        .find(&header)
        .unwrap_or_else(|| panic!("{file} has a `{name}` job"))
        + header.len();
    let rest = &workflow[start..];
    let end = rest
        .match_indices("\n  ")
        .map(|(at, _)| at)
        .find(|&at| rest[at + 3..].starts_with(|c: char| c != ' ' && c != '\n'))
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

fn ci_scripts_job() -> String {
    workflow_job("ci.yaml", "scripts")
}

/// The `#` comment block directly above the header of job `name`: the last paragraph
/// before it, which must consist of comment lines only.
fn job_lead_comment(file: &str, name: &str) -> String {
    let workflow = workflow(file);
    let header = format!("\n  {name}:\n");
    let before = &workflow[..workflow
        .find(&header)
        .unwrap_or_else(|| panic!("{file} has a `{name}` job"))];
    let comment = before.rsplit_once("\n\n").map_or(before, |(_, last)| last);
    for line in comment.lines().filter(|line| !line.trim().is_empty()) {
        assert!(
            line.starts_with("  #"),
            "the `{name}` job in {file} must sit directly under its explanatory comment block, not under: {line}"
        );
    }
    comment.to_string()
}

/// The one CI job that runs the scripts themselves: both linters checked, then the relay
/// smoke on all three runner families under a throwaway HOME with every tool directory
/// pointed under it, macOS on its own /bin/bash 3.2 and Windows under both PowerShells.
/// Service installation stays out of CI on purpose and the job says so.
#[test]
fn the_ci_scripts_job_lints_and_smokes_the_relay_on_every_runner_family() {
    let job = ci_scripts_job();

    for os in ["ubuntu-latest", "macos-latest", "windows-latest"] {
        assert!(
            job.contains(&format!("- {os}\n")),
            "the scripts job matrix must include {os}:\n{job}"
        );
    }

    let shellcheck = job
        .lines()
        .find(|line| line.trim_start().starts_with("shellcheck -S style"))
        .expect("the job runs `shellcheck -S style` on the shell scripts");
    for target in [
        "scripts/*.sh",
        "scripts/*/*.sh",
        "deployment/propagation-node/entrypoint.sh",
    ] {
        assert!(
            shellcheck.contains(target),
            "shellcheck must cover {target}: {shellcheck}"
        );
    }

    assert!(
        job.contains("PSSA_VERSION: 1.25.0"),
        "PSScriptAnalyzer is pinned at 1.25.0 in one step-level variable so a new rule is a reviewed bump, not a red PR"
    );
    assert!(
        job.contains("Install-Module PSScriptAnalyzer -RequiredVersion $env:PSSA_VERSION"),
        "PSScriptAnalyzer is installed at the pinned version"
    );
    assert!(
        job.contains("Import-Module PSScriptAnalyzer -RequiredVersion $env:PSSA_VERSION"),
        "the pinned PSScriptAnalyzer is the one imported (a newer module already on the image must not win)"
    );
    assert!(
        job.contains("$files = @(git ls-files '*.ps1')"),
        "the PowerShell files are enumerated from the git index, so a .ps1 anywhere in the tree (tests/fixtures/ has one) is analysed, not just scripts/"
    );
    assert!(
        job.contains("if (-not $files) { throw"),
        "an empty enumeration must fail the step, not analyse nothing and pass"
    );
    assert!(
        job.contains(
            "$files | ForEach-Object { Invoke-ScriptAnalyzer -Path $_ -Severity Warning,Error }"
        ),
        "every enumerated .ps1 is analysed at Warning,Error"
    );
    assert!(
        !job.contains("Invoke-ScriptAnalyzer -Path scripts"),
        "the analyser must not be narrowed back to a fixed directory"
    );
    assert!(
        job.contains("if ($r) { exit 1 }"),
        "a PSScriptAnalyzer finding must fail the lane, not scroll past"
    );

    // The throwaway HOME: every directory a rung may write to is under it, on both
    // smoke steps, so an install never leaks into the runner image.
    for key in [
        "PIPX_HOME",
        "PIPX_BIN_DIR",
        "UV_TOOL_DIR",
        "UV_TOOL_BIN_DIR",
    ] {
        let settings: Vec<&str> = job
            .lines()
            .filter(|line| line.trim_start().starts_with(&format!("{key}:")))
            .collect();
        assert_eq!(
            settings.len(),
            2,
            "{key} must be set on the Unix and the Windows smoke step: {settings:?}"
        );
        for setting in settings {
            assert!(
                setting.contains("${{ runner.temp }}") && setting.contains("mesh-home"),
                "{key} must live under the throwaway HOME: {setting}"
            );
        }
    }
    assert!(
        job.contains("HOME: ${{ runner.temp }}/mesh-home")
            && job.contains("USERPROFILE: ${{ runner.temp }}\\mesh-home"),
        "the relay runs under a throwaway HOME / USERPROFILE on every runner family"
    );

    assert!(
        job.contains("relay=(/bin/bash scripts/mesh-relay.sh)"),
        "the macOS leg runs the relay under the runner's /bin/bash (3.2), the bash the README one-liner lands in"
    );
    assert!(
        job.contains("shell: powershell") && job.contains("shell: pwsh"),
        "the Windows leg runs the relay under pwsh AND Windows PowerShell 5.1"
    );
    assert!(
        job.contains(
            "powershell -NoProfile -ExecutionPolicy Bypass -File scripts/mesh-relay.ps1 -NoService -DryRun -AllowRoot"
        ),
        "the 5.1 step must drive mesh-relay.ps1 itself, not just load the module"
    );
    assert!(
        job.contains("$ErrorActionPreference = 'Continue'"),
        "the 5.1 step must not turn the relay's stderr into a terminating error; $LASTEXITCODE is the oracle"
    );
    assert!(
        job.contains("shell: bash\n")
            && job.contains(
                "bash scripts/mesh-relay.sh --no-service --dry-run || rc=$?; [ \"$rc\" -eq 2 ]"
            ),
        "the Windows leg must prove the bash relay refuses Git Bash with exit 2"
    );
    assert!(
        job.contains(
            "Get-ScheduledTask -TaskName 'Coyote rnsd' -ErrorAction SilentlyContinue)) { throw"
        ),
        "the pwsh dry run must be proven to register no Scheduled Task"
    );
    assert!(
        job.contains("bash=/bin/bash")
            && job.contains("\"$bash\" scripts/install_coyote.sh --help"),
        "the macOS installer --help probe must run under the runner's /bin/bash"
    );
    assert!(
        job.contains("--version 1.5.1 --no-service --dry-run")
            && job.contains("'-Version', '1.5.1', '-NoService', '-DryRun'"),
        "a non-default version must land in the dry-run plan on both shells"
    );
    // GitHub's pwsh wrapper ends a step with `exit $LASTEXITCODE`, and the pwsh smoke
    // step's last relay call is the expected refusal; every Windows step ends with exit 0.
    for name in [
        "Mesh Relay Smoke (Windows)",
        "Mesh Relay Venv Plan Under The Roaming Profile (Windows)",
        "Mesh Relay Under Windows PowerShell 5.1",
    ] {
        let step = job
            .split("    - name: ")
            .find(|step| step.starts_with(name))
            .unwrap_or_else(|| panic!("ci.yaml has a `{name}` step"));
        // The slice runs up to the next `- name:`, so it carries that step's leading comment.
        let last_code_line = step
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'));
        assert_eq!(
            last_code_line,
            Some("        exit 0"),
            "the `{name}` step must end with `exit 0` so a relay's last exit code cannot fail it:\n{step}"
        );
    }

    // The venv fallback is proven on a PATH without uv or pipx and with neither config
    // variable set, under a profile the smoke step has not already installed into.
    for (needle, why) in [
        (
            "Remove-Item Env:COYOTE_CONFIG_DIR, Env:XDG_CONFIG_HOME",
            "the venv plan must be made with both config variables unset so APPDATA is the one left",
        ),
        (
            "APPDATA: ${{ runner.temp }}\\mesh-venv-home\\AppData\\Roaming",
            "APPDATA must be a throwaway under its own profile, not the smoke step's (that one already holds a working rnsd.cmd)",
        ),
        (
            "Join-Path $_ 'uv.exe')) -and -not (Test-Path (Join-Path $_ 'pipx.exe'))",
            "every PATH entry holding uv or pipx must be stripped so the venv rung is reached",
        ),
        (
            "Install: py -3 -m venv",
            "the plan must pick `py -3`, the rung before `python`",
        ),
        (
            "\\\\AppData\\\\Roaming\\\\coyote\\\\mesh\\\\rns-venv",
            "the plan must place the venv under %APPDATA%\\coyote\\mesh\\rns-venv",
        ),
    ] {
        assert!(
            job.contains(needle),
            "the Roaming-profile venv step lacks {needle:?}: {why}"
        );
    }

    // The dry-run and real-run smoke invocations and the checks on their results, each
    // present in both shells.
    for (what, needles) in [
        (
            "dry run",
            &["--no-service --dry-run", "'-NoService', '-DryRun'"][..],
        ),
        (
            "dry run changes nothing",
            &[
                "[ ! -e \"$HOME/.reticulum\" ]",
                "dry run created .reticulum",
            ],
        ),
        (
            "install + rnsd --version",
            &["\"$BIN_DIR/rnsd\" --version", "'rnsd.cmd') --version"],
        ),
        (
            "re-run hash unchanged",
            &["shasum -a 256", "Get-FileHash -Algorithm SHA256"],
        ),
        (
            "re-run reports the existing config",
            &["grep -q 'already exists'", "-notmatch 'already exists'"],
        ),
        (
            "pre-existing config byte-identical",
            &[
                "cmp \"$RUNNER_TEMP/expected-config\"",
                "an existing config was modified",
            ],
        ),
        (
            "stanzas printed",
            &[
                "grep -q '\\[\\[Team Relay\\]\\]'",
                "stanzas lack [[Team Relay]]",
            ],
        ),
        (
            "root refusal",
            &[
                "sudo -E env HOME=\"$HOME\"",
                "Administrator is refused without -AllowRoot",
            ],
        ),
    ] {
        for needle in needles {
            assert!(
                job.contains(needle),
                "the smoke matrix lacks the {what} check ({needle:?}):\n{job}"
            );
        }
    }

    let workflow = read(
        repo_root()
            .join(".github")
            .join("workflows")
            .join("ci.yaml"),
    );
    assert!(
        workflow.contains("Service installation is NOT exercised here"),
        "ci.yaml must say that the systemd/launchd/Scheduled-Task half is manual-VM acceptance, not CI"
    );
}

/// The image smoke runs in two places: informationally on every PR around the latest
/// released coyote (the only binary the Dockerfile can download for a PR), and as a gate
/// in the release workflow, where a host-arch build is smoked before the multi-platform
/// push so a broken rnsd layer or entrypoint never reaches Docker Hub.
#[test]
fn both_workflows_run_the_image_smoke_and_the_release_gates_the_push_on_it() {
    let ci = workflow_job("ci.yaml", "image-smoke");
    let ci_comment = job_lead_comment("ci.yaml", "image-smoke");
    for needle in [
        "runs-on: ubuntu-latest",
        "timeout-minutes:",
        "scripts/image-smoke.sh coyote-smoke:ci --pn coyote-pn:ci",
        "docker build -t coyote-pn:ci deployment/propagation-node",
        "gh release view --json tagName -q .tagName",
        "sed 's/^v//'",
        "GH_TOKEN: ${{ github.token }}",
        "--build-arg COYOTE_VERSION=",
        "permissions:\n      contents: read\n",
    ] {
        assert!(
            ci.contains(needle),
            "the ci.yaml image-smoke job must contain `{needle}`: it builds the Dockerfile around the latest release resolved through `gh`, builds the propagation node on its own so a failure there is attributable, and runs the smoke against both:\n{ci}"
        );
    }
    assert!(
        !ci.contains("continue-on-error"),
        "a red smoke must show as a red job, not vanish into a green run"
    );
    assert!(
        !ci.contains("cache-to"),
        "a GHA layer cache of the multi-gigabyte image would evict the rust-cache entries the `All` matrix depends on"
    );
    assert!(
        !ci.contains("needs:"),
        "the image smoke is informational and runs beside the other jobs, not after them"
    );
    for phrase in ["latest released coyote", "not the PR's Rust"] {
        assert!(
            ci_comment.contains(phrase),
            "the comment above the image-smoke job must say `{phrase}`, so nobody reads a green run as coverage of the PR's own binary:\n{ci_comment}"
        );
    }

    let release = workflow_job("release.yaml", "publish-sandbox-image");
    let smoke_at = release
        .find("scripts/image-smoke.sh coyote-smoke:release --pn coyote-pn:release")
        .expect("publish-sandbox-image smokes `coyote-smoke:release`");
    let push_at = release
        .find("platforms: linux/amd64,linux/arm64")
        .expect("publish-sandbox-image still has the multi-platform push");
    assert!(
        smoke_at < push_at,
        "the smoke must precede the multi-platform push: it is the gate, and a smoke after the push guards nothing"
    );
    let before_smoke = &release[..smoke_at];
    for needle in [
        "load: true",
        "push: false",
        "tags: coyote-smoke:release",
        "docker build --load -t coyote-pn:release deployment/propagation-node",
    ] {
        assert!(
            before_smoke.contains(needle),
            "`{needle}` must come before the smoke: both images are loaded into the daemon (the job's buildx container builder keeps a plain build in its own cache) and never pushed:\n{before_smoke}"
        );
    }
    assert!(
        before_smoke
            .lines()
            .any(|line| line.trim() == "platforms: linux/amd64"),
        "the smoke build is host-arch only (`platforms: linux/amd64`), so the smoke never executes under QEMU:\n{before_smoke}"
    );
    let from_push = &release[push_at..];
    for needle in [
        "push: ${{ env.ACT != 'true' }}",
        "tags: darkalex17/coyote:latest, darkalex17/coyote:v${{ env.version }}",
    ] {
        assert!(
            from_push.contains(needle),
            "the push step must stay as it was, with `{needle}`:\n{from_push}"
        );
    }
    assert_eq!(
        release
            .matches("build-args: COYOTE_VERSION=${{ env.version }}")
            .count(),
        2,
        "the smoke build and the push take the same `COYOTE_VERSION`, so the smoked image is the pushed image:\n{release}"
    );
    assert!(
        !release.contains("continue-on-error"),
        "a failed smoke must fail the release, not be noted and pushed anyway"
    );
    assert!(
        !release.contains("cache-to"),
        "the GHA cache is repository-scoped: a layer cache of the image here would evict the same rust-cache entries the `All` matrix depends on"
    );

    let tracking_id = Regex::new(r"(?i)\b(task|plan|scope)-[0-9A-Z]").unwrap();
    for (what, text) in [
        ("ci.yaml image-smoke", ci.as_str()),
        ("ci.yaml image-smoke comment", ci_comment.as_str()),
        ("release.yaml publish-sandbox-image", release.as_str()),
    ] {
        for line in text.lines() {
            assert!(
                !tracking_id.is_match(line).unwrap(),
                "{what} references plan/task tracking; that belongs in commit messages: {line}"
            );
        }
    }
}

/// The comment above `ingress_control = No` is copied from the propagation-node config,
/// so the two files explain the same setting in the same words; and the two relay
/// scripts write the same config body and print the same firewall sentence, so a
/// Linux and a Windows host end up with daemons that behave alike.
#[test]
fn relay_scripts_are_config_twins_and_copy_the_propagation_node_comment() {
    let pn_config = read(
        repo_root()
            .join("deployment")
            .join("propagation-node")
            .join("reticulum.config"),
    );
    let pn_lines: Vec<&str> = pn_config.lines().collect();
    let ingress_at = pn_lines
        .iter()
        .position(|line| line.trim() == "ingress_control = No")
        .expect("the propagation-node config turns ingress_control off");
    let pn_comment: Vec<&str> = pn_lines[..ingress_at]
        .iter()
        .rev()
        .take_while(|line| line.trim_start().starts_with('#'))
        .map(|line| line.trim())
        .collect();
    assert!(
        !pn_comment.is_empty(),
        "the propagation-node config explains ingress_control in a comment above it"
    );

    let sh = read(scripts_dir().join("mesh-relay.sh"));
    let ps1 = read(scripts_dir().join("mesh-relay.ps1"));
    for (name, script) in [("mesh-relay.sh", &sh), ("mesh-relay.ps1", &ps1)] {
        for line in &pn_comment {
            assert!(
                script.lines().any(|l| l.trim() == *line),
                "{name} must carry the propagation-node's ingress_control comment verbatim: {line:?}"
            );
        }
        // Both scripts emit the same stanzas and settings.
        for needle in [
            "enable_transport = True",
            "forwards traffic for any other Reticulum peer it hears on its interfaces",
            "[logging]",
            "loglevel = 4",
            "[[Coyote Local]]",
            "type = AutoInterface",
            "[[Coyote Sessions]]",
            "type = TCPServerInterface",
            "listen_ip = 127.0.0.1",
            "listen_port = 4242",
            "ingress_control = No",
            "[[Team Relay]]",
            "type = TCPClientInterface",
        ] {
            assert!(
                script.contains(needle),
                "{name} config text lacks {needle:?}"
            );
        }
    }

    // One firewall sentence, up to the per-OS tail.
    let firewall = "Firewall: AutoInterface listens for LAN peers, and with enable_transport this rnsd forwards traffic for any Reticulum peer on the LAN (and on to the Team Relay when one is configured).";
    for (name, script) in [("mesh-relay.sh", &sh), ("mesh-relay.ps1", &ps1)] {
        assert!(
            script.contains(firewall),
            "{name} must print the shared firewall warning (a transport node forwards for the LAN and on to the relay): {firewall}"
        );
    }

    // The scripts the Windows runner checks out must stay LF, and so must the workflow.
    let attributes = read(repo_root().join(".gitattributes"));
    for pattern in [
        "scripts/*.sh",
        "scripts/**/*.ps1",
        "tests/**/*.ps1",
        ".github/workflows/*.yaml",
    ] {
        assert!(
            attributes
                .lines()
                .any(|line| line.trim() == format!("{pattern} text eol=lf")),
            ".gitattributes must pin `{pattern} text eol=lf`: a CRLF checkout breaks bash and the LF pins above"
        );
    }
}

/// The comment under `enable_transport = True` says why Coyote's heartbeat is hourly in
/// the same words in both relay scripts and the image template: it names the Reticulum
/// announce-rate defaults and their source, and tells the operator not to lower
/// `announce_rate_target` on an interface nor `default_ar_target` in the `[reticulum]`
/// section. The period it quotes is `HEARTBEAT_SECS`, and the slack it quotes as `a
/// minute` is `HEARTBEAT_SLACK_SECS`.
#[test]
fn relay_configs_explain_the_hourly_heartbeat_in_the_same_words() {
    let announce = read(repo_root().join("src").join("mesh").join("announce.rs"));
    let const_u64 = |name: &str| -> u64 {
        announce
            .lines()
            .find_map(|line| line.strip_prefix(&format!("pub(crate) const {name}: u64 = ")))
            .unwrap_or_else(|| panic!("src/mesh/announce.rs defines {name}"))
            .trim_end_matches(';')
            .parse()
            .unwrap_or_else(|_| panic!("{name} is a literal"))
    };
    let heartbeat_secs = const_u64("HEARTBEAT_SECS");
    let slack_secs = const_u64("HEARTBEAT_SLACK_SECS");

    let blocks: Vec<(&str, Vec<String>)> =
        ["mesh-relay.sh", "mesh-relay.ps1", "reticulum.config.tmpl"]
            .into_iter()
            .map(|name| {
                let text = read(scripts_dir().join(name));
                let lines: Vec<&str> = text.lines().map(str::trim).collect();
                let at = lines
                    .iter()
                    .position(|line| *line == "enable_transport = True")
                    .unwrap_or_else(|| panic!("{name} does not write enable_transport = True"));
                let block: Vec<String> = lines[at + 1..]
                    .iter()
                    .skip_while(|line| line.is_empty())
                    .take_while(|line| line.starts_with('#'))
                    .map(|line| line.to_string())
                    .collect();
                (name, block)
            })
            .collect();

    let (first, reference) = &blocks[0];
    for (name, block) in &blocks[1..] {
        assert_eq!(
            block, reference,
            "{name}'s heartbeat comment differs from {first}'s"
        );
    }

    let prose = reference.join(" ");
    for needle in [
        "announce_rate_target",
        "announce_rate_grace",
        "default_ar_target",
        "DEFAULT_AR_TARGET",
        "DEFAULT_AR_GRACE",
        "Interface.py",
        "hourly",
        &format!("{heartbeat_secs} s"),
        "a minute of slack",
    ] {
        assert!(
            prose.contains(needle),
            "the heartbeat comment does not say {needle:?}: {prose}"
        );
    }
    assert_eq!(heartbeat_secs, 3600, "the comment says `hourly`");
    assert_eq!(slack_secs, 60, "the comment says `a minute of slack`");
}

/// The relay scripts and the image template describe Reticulum's announce-rate defaults;
/// none of them sets one, neither the per-interface `announce_rate_*` keys nor the
/// `[reticulum]`-section `default_ar_*` keys that set the defaults those fall back to. A
/// relay that lowered `announce_rate_target` would take a faster heartbeat the community
/// relays do not, and one that raised it would drop the hourly heartbeat the comment above
/// it promises works.
#[test]
fn relay_configs_leave_the_announce_rate_settings_at_the_reticulum_default() {
    for name in ["mesh-relay.sh", "mesh-relay.ps1", "reticulum.config.tmpl"] {
        let text = read(scripts_dir().join(name));
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            for setting in [
                "announce_rate_target",
                "announce_rate_grace",
                "announce_rate_penalty",
                "default_ar_target",
                "default_ar_grace",
                "default_ar_penalty",
            ] {
                assert!(
                    !line.starts_with(setting),
                    "{name}:{} sets {setting}; the Reticulum default is the pinned value: {line}",
                    index + 1
                );
            }
        }
    }
}

/// Every download the installers make is HTTPS over TLS 1.2 or newer and stays HTTPS
/// across redirects (curl refuses an https-to-http hop, the PowerShell installer refuses a
/// non-https asset URL, wget gets whichever TLS flags it understands), and the relay
/// script they fall back to is fetched from the repository over HTTPS. The bash prompt
/// gate reads both a TTY stdin and stdout so `curl | bash` never blocks.
#[test]
fn installers_pin_https_tls12_and_gate_the_prompt_on_a_tty() {
    let sh = read(scripts_dir().join("install_coyote.sh"));
    for (line_no, line) in sh.lines().enumerate() {
        // An invocation, not the one-liner quoted in comments and advice lines.
        let invocation = line
            .trim_start()
            .trim_start_matches("if ")
            .trim_start_matches("! ");
        if invocation.starts_with("curl ") {
            assert!(
                invocation.contains("--proto '=https'")
                    && invocation.contains("--proto-redir '=https'")
                    && invocation.contains("--tlsv1.2"),
                "install_coyote.sh:{} downloads with curl but not pinned to HTTPS (also across redirects) + TLS 1.2: {line}",
                line_no + 1
            );
        }
        if invocation.starts_with("wget ") {
            assert!(
                invocation.contains("${WGET_TLS[@]+\"${WGET_TLS[@]}\"}"),
                "install_coyote.sh:{} downloads with wget without the feature-detected TLS flags: {line}",
                line_no + 1
            );
        }
    }
    let detect = sh
        .find("detect_wget_tls()")
        .map(|at| &sh[at..])
        .expect("install_coyote.sh feature-detects wget's TLS flags");
    let detect = &detect[..detect.find("\n}\n").unwrap()];
    for flag in ["--https-only", "--secure-protocol=TLSv1_2"] {
        assert!(
            detect.contains(flag),
            "detect_wget_tls must offer {flag} when wget advertises it:\n{detect}"
        );
    }
    assert!(
        sh.contains(
            "https://raw.githubusercontent.com/${REPO}/refs/heads/main/scripts/mesh-relay.sh"
        ),
        "the fallback relay script is fetched from the repository over HTTPS"
    );
    assert!(
        sh.contains("-t 0 && -t 1") || sh.contains("[ -t 0 ] && [ -t 1 ]"),
        "the bash prompt gate must require a TTY on both stdin and stdout so a piped install never waits on a question"
    );

    let ps1 = read(scripts_dir().join("install_coyote.ps1"));
    let tls_at = ps1
        .find("[Net.SecurityProtocolType]::Tls12")
        .expect("install_coyote.ps1 enables TLS 1.2");
    for call in ["Invoke-WebRequest", "Invoke-RestMethod"] {
        for (at, _) in ps1.match_indices(call) {
            assert!(
                at > tls_at,
                "install_coyote.ps1 calls {call} before TLS 1.2 is enabled"
            );
            let line_end = ps1[at..].find('\n').map(|n| at + n).unwrap_or(ps1.len());
            let line = &ps1[at..line_end];
            assert!(
                line.contains("-UseBasicParsing"),
                "install_coyote.ps1: {call} must pass -UseBasicParsing for Windows PowerShell 5.1 without IE: {line}"
            );
        }
    }
    let guard = ps1
        .find(".StartsWith('https://')")
        .expect("install_coyote.ps1 refuses a browser_download_url that is not https");
    let download = ps1
        .find("-Uri $asset.browser_download_url")
        .expect("install_coyote.ps1 downloads the asset by its browser_download_url");
    assert!(
        guard < download,
        "the https guard must run before the asset is downloaded"
    );
    assert!(
        ps1.contains("https://raw.githubusercontent.com/$Repo/refs/heads/main/scripts")
            && ps1.contains("mesh-relay.ps1"),
        "the fallback relay script is fetched from the repository's scripts/ over HTTPS"
    );
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
        sh.contains("READY_TIMEOUT=\"${COYOTE_MESH_READY_TIMEOUT:-30}\""),
        "mesh-relay.sh must wait 30 s for the listener (the documented exit 3 contract), with the override reserved for the test harness"
    );
    let ps1 = read(scripts_dir().join("mesh-relay.ps1"));
    assert!(
        ps1.contains("$ReadyTimeoutSeconds = if ($env:COYOTE_MESH_READY_TIMEOUT) { [int]$env:COYOTE_MESH_READY_TIMEOUT } else { 30 }"),
        "mesh-relay.ps1 must wait 30 s for the listener (the documented exit 3 contract), with the override reserved for the test harness"
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
    for (name, text) in [("install_coyote.sh", &sh), ("install_coyote.ps1", &ps1)] {
        assert!(
            text.contains("run it as your normal user"),
            "{name} must tell root/Administrator to run the relay unelevated instead of prompting"
        );
    }
    let root_check = sh
        .find("if [[ \"$(id -u)\" -eq 0 ]]; then\n      mesh_pointer")
        .expect("install_coyote.sh points root at the relay without a prompt");
    let tty_check = sh
        .find("elif [[ -t 0 && -t 1 ]]; then")
        .expect("install_coyote.sh gates the prompt on a TTY");
    assert!(
        root_check < tty_check,
        "root must be checked before the TTY, so root at a terminal is never prompted"
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
    let xml_escape = sh
        .split("xml_escape() {")
        .nth(1)
        .and_then(|rest| rest.split("\n}").next())
        .expect("mesh-relay.sh defines xml_escape");
    assert!(
        xml_escape.contains("| sed "),
        "xml_escape must escape through sed: bash 3.2 keeps quotes inside a ${{text//x/y}} replacement literally and bash 5.2 reads an unquoted `&` there as the matched text:\n{xml_escape}"
    );
    assert!(
        !sh.contains("# shellcheck"),
        "mesh-relay.sh must lint clean without shellcheck directives; fix the finding instead of disabling it"
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
    let attributes = read(repo_root().join(".gitattributes"));
    for pattern in ["Dockerfile", "scripts/*.sh", "scripts/*.tmpl"] {
        assert!(
            attributes
                .lines()
                .any(|line| line.trim() == format!("{pattern} text eol=lf")),
            ".gitattributes must pin `{pattern} text eol=lf`: the LF sweep below, the two image scripts' shebang lines and the entrypoint's `#@if` markers all need an LF checkout on the Windows runners"
        );
    }
    let paths: Vec<PathBuf> = RELAY_SCRIPTS
        .iter()
        .chain(INSTALLERS.iter())
        .chain(IMAGE_SCRIPTS.iter())
        .map(|name| scripts_dir().join(name))
        .chain([repo_root().join("Dockerfile")])
        .collect();
    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy();
        let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
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
        for name in ["mesh-relay.sh", "docker-entrypoint.sh", "image-smoke.sh"] {
            let metadata = fs::metadata(scripts_dir().join(name)).unwrap();
            assert!(
                metadata.permissions().mode() & 0o111 != 0,
                "{name} should be executable in the tree so `./scripts/{name}` works from a checkout"
            );
        }
    }
}

#[test]
fn the_image_dockerfile_installs_the_propagation_node_rns_version() {
    let version = dockerfile_rns_version();
    let dockerfile = read(repo_root().join("Dockerfile"));
    let mut lines = dockerfile.lines();
    assert_eq!(
        lines.next(),
        Some("# syntax=docker/dockerfile:1"),
        "Dockerfile must start with the syntax directive; the check directive below is ignored without it"
    );
    assert_eq!(
        lines.next(),
        Some("# check=error=true"),
        "Dockerfile must fail the build on BuildKit check warnings instead of printing them"
    );
    assert!(
        dockerfile.contains(&format!("ARG RNS_VERSION={version}")),
        "Dockerfile must declare `ARG RNS_VERSION={version}`: deployment/propagation-node/Dockerfile says so and that is the version the interop harness verified; bump both together"
    );
    assert!(
        dockerfile.contains("\"rns==${RNS_VERSION}\""),
        "Dockerfile must install rns pinned to RNS_VERSION rather than the latest release"
    );
    assert!(
        dockerfile.contains("uv tool install"),
        "Dockerfile must install rns as a uv tool so rnsd lands in /home/agent/.local/bin, already on PATH"
    );
    assert!(
        dockerfile.contains("--no-build"),
        "Dockerfile must pass --no-build so a missing wheel fails the build instead of compiling under QEMU on arm64"
    );
    assert!(
        dockerfile.contains("--python 3.12"),
        "Dockerfile must install rns under a uv-managed CPython 3.12, the interpreter the interop harness verified rns on"
    );
    assert!(
        dockerfile.contains("UV_NO_CACHE=1"),
        "Dockerfile must disable the uv cache for the rns install so no cache directory rides into the flattened image"
    );
    assert!(
        dockerfile.contains(
            "ENTRYPOINT [\"/usr/bin/tini\", \"-s\", \"-g\", \"--\", \"/usr/local/bin/coyote-entrypoint\"]"
        ),
        "Dockerfile must run tini with -s and -g: -s keeps tini reaping (and quiet) when it is not PID 1, e.g. under `docker run --init` (PID 1 reaps orphans regardless); -g delivers TERM/INT to the process group, which is how the foreground main command receives them"
    );
    assert!(
        dockerfile.contains("scripts/reticulum.config.tmpl /opt/coyote/reticulum.config.tmpl"),
        "Dockerfile must bake the Reticulum template at /opt/coyote/reticulum.config.tmpl, the path the entrypoint renders from"
    );
    assert!(
        dockerfile.contains("install -d -m 0755 /opt/coyote"),
        "Dockerfile must create /opt/coyote with install -d: left to COPY it takes the file mode and uid 1000 cannot traverse it"
    );
    assert!(
        !dockerfile.contains("PYTHONUNBUFFERED"),
        "PYTHONUNBUFFERED stays scoped to the rnsd child in the entrypoint, never an image-wide ENV"
    );
    assert!(
        !dockerfile.contains("HEALTHCHECK"),
        "Dockerfile must declare no HEALTHCHECK: the image runs as a sandbox and a CLI, not a service"
    );
}

/// The entrypoint's first write of `~/.reticulum/config` is permanent for that
/// container's volume, so the template must carry the `[logging]` section without
/// which `rnsd -v` is ignored, and the entrypoint must start rnsd unbuffered
/// (`RNS.log` is a bare `print()`) under `env -i` in its own session (neither a TTY
/// SIGINT nor tini's group signal may reach it: rnsd exits on SIGINT,
/// `Reticulum.py:375`), run the main command in its foreground (a dash `&` child
/// inherits SIGINT ignored and `/dev/null` on fd 0) without ever `exec`ing it, and
/// trap every signal tini -g forwards with a command rather than `''` (an ignored
/// signal would be inherited by the main command).
#[test]
fn the_image_template_and_entrypoint_carry_the_rnsd_logging_contract() {
    let template = read(scripts_dir().join("reticulum.config.tmpl"));
    for needle in [
        "[logging]",
        "loglevel = 4",
        "ingress_control = No",
        "listen_ip = 127.0.0.1",
        "enable_transport = True",
        "#@if lan",
        "#@if relay",
    ] {
        assert!(
            template.contains(needle),
            "reticulum.config.tmpl must contain `{needle}`: the rendered config is written once and never overwritten"
        );
    }
    assert!(
        !template.contains("share_instance = No"),
        "reticulum.config.tmpl must not write `share_instance = No`: rnstatus and rnpath inside the container attach to the shared instance"
    );

    let entrypoint = read(scripts_dir().join("docker-entrypoint.sh"));
    assert_eq!(
        entrypoint.lines().next(),
        Some("#!/bin/sh"),
        "docker-entrypoint.sh must be POSIX sh: the image's /bin/sh is dash and shellcheck infers the dialect from the shebang"
    );
    for needle in [
        "PYTHONUNBUFFERED=1 setsid rnsd -vv",
        "env -i HOME=",
        "COYOTE_MESH_RNSD",
        "COYOTE_MESH_RELAY",
        "COYOTE_MESH_LAN",
        "trap ':' HUP INT QUIT TERM USR1 USR2",
        "[ ! -e \"$config_dir/config\" ]",
        "Reticulum.py:459-467",
        "RNS/__init__.py:129-134",
        "Reticulum.py:375",
    ] {
        assert!(
            entrypoint.contains(needle),
            "docker-entrypoint.sh must contain `{needle}`: the smoke's log assertions, the env contract (PYTHONUNBUFFERED scoped to the rnsd child under env -i), the never-overwrite guard and the signal model depend on it"
        );
    }
    assert!(
        entrypoint.contains(r#"{ sub(/\r$/, "") }"#),
        "docker-entrypoint.sh's awk program must strip a trailing CR from every line before the `#@if` markers are matched: a CRLF template would otherwise render every optional stanza unconditionally"
    );
    assert!(
        !entrypoint.contains("exec coyote"),
        "docker-entrypoint.sh must run coyote as a child, not exec it: the script has to outlive it to stop rnsd and return its exit code"
    );
    for needle in [r#"/proc/"$rnsd_pid"/stat"#, r#"[ "$ppid" = "$$" ]"#] {
        assert!(
            entrypoint.contains(needle),
            "docker-entrypoint.sh must identify its rnsd child by parent pid (`{needle}`): a comm-based check reads `env` or `setsid` on the not-yet-exec'd child and skips the TERM a main command that returns at once still owes it; the ppid survives every exec and setsid()"
        );
    }
    assert!(
        entrypoint.contains("${stat##*) }"),
        "docker-entrypoint.sh must strip the stat line through its LAST `) ` with parameter expansion: comm may hold spaces or `)`, and a `$(awk ...)` subshell (traps reset to default) hit by a group signal mid-stop would report a live rnsd as dead"
    );
    assert!(
        entrypoint.contains("did not exit within 5 s after TERM; sending KILL"),
        "docker-entrypoint.sh must say so on stderr before the KILL escalation: a daemon that ignored TERM is otherwise indistinguishable from one that exited in time"
    );
    assert!(
        !entrypoint.contains(r#"/proc/"$rnsd_pid"/comm"#),
        "docker-entrypoint.sh must not decide `ours` by /proc/<pid>/comm: see the ppid pin above"
    );
    assert!(
        entrypoint.contains("  \"$@\"\nelse\n  coyote \"$@\"\nfi\nrc=$?"),
        "docker-entrypoint.sh must dispatch the main command in the foreground and read its status straight from $?: dash hard-ignores SIGINT in an `&` child and gives it /dev/null as stdin, so a background job plus `wait` would make `docker run -it` uninterruptible and starve the command of the container's stdin"
    );
    assert!(
        !entrypoint.contains("sed -i"),
        "docker-entrypoint.sh must never edit a config in place: the rendered file is written once through mktemp + mv and an existing one is never touched"
    );
    assert!(
        !entrypoint.contains("# shellcheck"),
        "docker-entrypoint.sh must lint clean without shellcheck directives; fix the finding instead of disabling it"
    );
}

#[test]
fn the_image_smoke_asserts_the_post_connect_relay_line() {
    let smoke = read(scripts_dir().join("image-smoke.sh"));
    assert_eq!(
        smoke.lines().next(),
        Some("#!/usr/bin/env bash"),
        "image-smoke.sh is bash (/dev/tcp, [[ ]], here-strings)"
    );
    assert!(
        smoke.contains(
            "TCP connection for TCPInterface\\[Team Relay/[^]]*\\] established|Reconnected socket for TCPInterface\\[Team Relay/"
        ),
        "image-smoke.sh must match the post-connect line (`] established`) or the reconnect line: a bare `TCP connection for TCPInterface[Team Relay` matches the pre-connect line and could never fail"
    );
    for needle in [
        "TCPInterface.py:233",
        ":247 logs",
        "gate at :290",
        ":291",
        "rnstatus",
        "COYOTE_MESH_RNSD=0",
        "/dev/tcp/127.0.0.1/4242",
    ] {
        assert!(
            smoke.contains(needle),
            "image-smoke.sh must contain `{needle}`: the regex citations, the in-container diagnostic on failure, and the rnsd-less prober"
        );
    }
    assert!(
        !smoke.contains("# shellcheck"),
        "image-smoke.sh must lint clean without shellcheck directives; fix the finding instead of disabling it"
    );
}

/// The GPL-2.0 and BSD 3-Clause texts the mesh dependencies rely on ship in the repository
/// root, NOTICE and CREDITS.md point at both files by name, and NOTICE no longer records the
/// texts as a release blocker.
#[test]
fn notice_and_credits_name_the_shipped_gpl_and_bsd_license_texts() {
    let gpl = read(repo_root().join("LICENSE-GPL-2.0"));
    for needle in ["GNU GENERAL PUBLIC LICENSE", "Version 2, June 1991"] {
        assert!(
            gpl.contains(needle),
            "LICENSE-GPL-2.0 must be the GPL version 2 text (lacks `{needle}`)"
        );
    }
    let bsd = read(repo_root().join("LICENSE-BSD-3-Clause"));
    assert!(
        bsd.contains("Redistribution and use in source and binary forms"),
        "LICENSE-BSD-3-Clause must carry the BSD license text"
    );
    for holder in [
        "isis agora lovecruft",
        "Henry de Valence",
        "DebugSteven",
        "Isis Agora Lovecruft, Henry de Valence",
    ] {
        assert!(
            bsd.contains(holder),
            "LICENSE-BSD-3-Clause must reproduce the copyright line naming `{holder}`"
        );
    }
    for crate_name in [
        "curve25519-dalek",
        "ed25519-dalek",
        "x25519-dalek",
        "subtle",
    ] {
        assert!(
            bsd.contains(&format!("{crate_name}:")),
            "LICENSE-BSD-3-Clause must label the `{crate_name}` notices"
        );
    }
    for (name, text) in [
        ("NOTICE", read(repo_root().join("NOTICE"))),
        ("CREDITS.md", read(repo_root().join("CREDITS.md"))),
    ] {
        for file in ["LICENSE-GPL-2.0", "LICENSE-BSD-3-Clause"] {
            assert!(text.contains(file), "{name} must point at {file}");
        }
    }
    let notice = read(repo_root().join("NOTICE"));
    for stale in ["release-blocking", "blocks a release", "Two obligations"] {
        assert!(
            !notice.contains(stale),
            "NOTICE still says `{stale}`: the license texts ship, so one obligation remains"
        );
    }
}

/// The crate Coyote depends on for the Reticulum transport: its own published fork.
const TRANSPORT_CRATE: &str = "coyote-rns-transport";
/// The upstream crate the fork tracks. Documents may name it only as the fork's origin.
const UPSTREAM_TRANSPORT_CRATE: &str = "reticulum-rs-transport";
const FORK_REPO: &str = "https://github.com/Dark-Alex-17/coyote-rns-transport";

/// The version `Cargo.toml` pins for the dependency `name`, read from its plain
/// `name = "x.y.z"` line; the mesh crates are pinned in that form on purpose.
fn cargo_toml_dependency_version(cargo_toml: &str, name: &str) -> String {
    let prefix = format!("{name} = \"");
    cargo_toml
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_string)
        .unwrap_or_else(|| panic!("Cargo.toml has no `{name} = \"<version>\"` line"))
}

/// A `[[package]]` entry of `Cargo.lock`: its version and the source it resolves from
/// (`None` for a workspace member, which carries no `source` line).
struct LockPackage {
    version: String,
    source: Option<String>,
}

/// Every `[[package]]` named `name` in `Cargo.lock`, in file order.
fn lock_packages(lock: &str, name: &str) -> Vec<LockPackage> {
    let header = format!("name = \"{name}\"");
    let lines: Vec<&str> = lock.lines().collect();
    let field = |line: &str, key: &str| {
        line.strip_prefix(&format!("{key} = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .map(str::to_string)
    };
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| **line == header)
        .map(|(at, _)| LockPackage {
            version: lines
                .get(at + 1)
                .and_then(|line| field(line, "version"))
                .unwrap_or_else(|| panic!("Cargo.lock package `{name}` has no version line")),
            source: lines.get(at + 2).and_then(|line| field(line, "source")),
        })
        .collect()
}

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
const MESH_CRATES: [&str; 3] = [TRANSPORT_CRATE, "lxmf-wire", "reticulum-rs-core"];

/// Every `*.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

/// The two mesh crates share `reticulum-rs-core`, so they are pinned on one upstream line
/// (the same `major.minor`; the fork's patch number moves on its own when a Coyote-only
/// patch is published, while `lxmf-wire` stays at the upstream release), the lockfile
/// carries each at exactly the version `Cargo.toml` pins for it and resolves exactly one
/// core, and all three come from crates.io, which rules out a patch table or a path
/// replacement wherever `Cargo.toml` or `Cargo.lock` might declare it (a source
/// replacement in a cargo config file is outside what these two files show). The upstream
/// transport crate is gone from the graph, and the pin block names the fork and the one
/// thing it changes.
#[test]
fn the_mesh_crates_move_together_and_resolve_one_core() {
    let cargo_toml = read(repo_root().join("Cargo.toml"));
    let transport = cargo_toml_dependency_version(&cargo_toml, TRANSPORT_CRATE);
    let wire = cargo_toml_dependency_version(&cargo_toml, "lxmf-wire");
    let line = |version: &str| -> String {
        let mut parts = version.split('.');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(major), Some(minor), Some(_), None) => format!("{major}.{minor}"),
            _ => panic!("the mesh crates are pinned as exact `major.minor.patch`, not `{version}`"),
        }
    };
    assert_eq!(
        line(&transport),
        line(&wire),
        "`{TRANSPORT_CRATE}` {transport} and `lxmf-wire` {wire} must track one upstream line (same major.minor): the fork's patch number may differ, the line may not"
    );
    let lock = read(repo_root().join("Cargo.lock"));
    for crate_name in MESH_CRATES {
        let packages = lock_packages(&lock, crate_name);
        assert_eq!(
            packages.len(),
            1,
            "Cargo.lock must resolve exactly one `{crate_name}`"
        );
        assert_eq!(
            packages[0].source.as_deref(),
            Some(CRATES_IO),
            "Cargo.lock must resolve `{crate_name}` from crates.io, not a patch, path or git source"
        );
    }
    for (crate_name, pinned) in [(TRANSPORT_CRATE, &transport), ("lxmf-wire", &wire)] {
        assert_eq!(
            &lock_packages(&lock, crate_name)[0].version,
            pinned,
            "Cargo.lock must carry `{crate_name}` at the version Cargo.toml pins"
        );
    }
    assert!(
        lock_packages(&lock, UPSTREAM_TRANSPORT_CRATE).is_empty(),
        "the upstream `{UPSTREAM_TRANSPORT_CRATE}` must not be in the graph beside the fork"
    );
    let manifest_code: String = cargo_toml
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in ["[patch", "git = \"", "vendor/"] {
        assert!(
            !manifest_code.contains(forbidden),
            "Cargo.toml must not carry `{forbidden}`: patches to the transport are published in the fork"
        );
    }
    for needle in [
        FORK_REPO,
        "crates/libs/rns-transport/README.md",
        "next_watchdog_deadline.rs",
        "rebasing the fork",
    ] {
        assert!(
            cargo_toml.contains(needle),
            "the Cargo.toml pin block must name `{needle}`"
        );
    }
}

/// Every document that explains the dependency names the fork as the transport Coyote
/// uses and the upstream crate only as the fork's origin, and the policy section CI links
/// keeps its heading.
#[test]
fn the_docs_name_the_fork_as_the_transport_and_the_upstream_only_as_its_origin() {
    let contributing = read(repo_root().join("CONTRIBUTING.md"));
    assert!(
        contributing.contains("\n## Dependency policy\n"),
        "CONTRIBUTING.md must keep the `## Dependency policy` heading the CI anchor points at"
    );
    assert!(
        workflow("ci.yaml").contains("CONTRIBUTING.md#dependency-policy"),
        "ci.yaml must still send readers to CONTRIBUTING.md#dependency-policy"
    );
    for needle in [
        FORK_REPO,
        "`vendor/`",
        "`[patch.crates-io]`",
        "cargo install coyote-ai",
        "`lxmf-wire` and `reticulum-rs-core` stay upstream",
    ] {
        assert!(
            contributing.contains(needle),
            "the CONTRIBUTING.md fork paragraph must say `{needle}`"
        );
    }
    for name in ["NOTICE", "CREDITS.md"] {
        let text = read(repo_root().join(name));
        for needle in [TRANSPORT_CRATE, FORK_REPO, "EPL-2.0 OR GPL-2.0-or-later"] {
            assert!(text.contains(needle), "{name} must name `{needle}`");
        }
    }
    for name in [
        "Cargo.toml",
        "CONTRIBUTING.md",
        "NOTICE",
        "CREDITS.md",
        "scripts/mesh-interop/README.md",
    ] {
        let text = read(repo_root().join(name));
        for paragraph in text.split("\n\n") {
            if paragraph.contains(UPSTREAM_TRANSPORT_CRATE) {
                assert!(
                    paragraph.contains("fork"),
                    "{name} names `{UPSTREAM_TRANSPORT_CRATE}` without calling it the fork's origin:\n{paragraph}"
                );
            }
        }
    }
}

/// The interop verification record names each of the three mesh crates next to the
/// version the lockfile resolves it at, carries no other crate version, says which host
/// it was taken on and which rows that host cannot run, keeps the sentence that defers
/// those rows to the CI matrix and the Linux job, and the CI job it defers them to exists.
#[test]
fn the_interop_record_names_the_pinned_mesh_crates() {
    let lock = read(repo_root().join("Cargo.lock"));
    let readme = read(scripts_dir().join("mesh-interop").join("README.md"));
    let record = readme
        .split_once("### Verification record")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("\n## ").map(|(record, _)| record))
        .expect("scripts/mesh-interop/README.md has a `### Verification record` section");
    let mut versions = Vec::new();
    for crate_name in MESH_CRATES {
        let version = lock_packages(&lock, crate_name)
            .pop()
            .unwrap_or_else(|| panic!("Cargo.lock resolves `{crate_name}`"))
            .version;
        let named = Regex::new(&format!(
            "`{}`[^`]*`{}`",
            fancy_regex::escape(crate_name),
            fancy_regex::escape(&version)
        ))
        .unwrap();
        assert!(
            named.is_match(record).unwrap(),
            "the verification record must name `{crate_name}` next to its locked version `{version}`"
        );
        versions.push(version);
    }
    let rns = dockerfile_rns_version();
    let semver = Regex::new(r"`(\d+\.\d+\.\d+)`").unwrap();
    for found in semver.captures_iter(record) {
        let found = found.unwrap().get(1).unwrap().as_str();
        assert!(
            versions.iter().any(|version| version == found)
                || found == rns
                || record.contains(&format!("LXMF `{found}` (the two commits pinned above)")),
            "the verification record carries a version `{found}` that is neither a locked mesh crate nor a Python pin"
        );
    }
    assert!(
        record.contains("`Mesh Interop` job"),
        "the verification record must say which CI job runs the Linux rows"
    );
    for needle in [
        ", on macOS (the netns rows do not exist on this host; they and the\nLinux run of row 1 are covered by the `Mesh Interop` CI job)",
        "Rows 2 and 3 also run on all three legs of the CI matrix, and row 1 runs only in the Linux\n`Mesh Interop` job; none of those runs is part of this record.",
    ] {
        assert!(
            record.contains(needle),
            "the verification record must keep its host clause and its CI-coverage sentence; missing:\n{needle}"
        );
    }
    assert!(
        workflow("ci.yaml").contains("\n  mesh-interop:\n"),
        "ci.yaml must still have the `mesh-interop` job the record defers to"
    );
    assert!(
        workflow("ci.yaml").contains("    name: Mesh Interop\n"),
        "ci.yaml must still name the interop job `Mesh Interop`"
    );
}

/// `text` as the scanner reads it: each run of consecutive `//` comment lines folded into
/// one entry with the markers stripped (so a name on one line and its version on the next
/// are read together), every other line as itself, each with the line it starts on.
fn folded_comment_runs(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut in_comment = false;
    for (at, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let body = ["///", "//!", "//"]
            .iter()
            .find_map(|marker| trimmed.strip_prefix(marker));
        match body {
            Some(body) if in_comment => {
                let run = out.last_mut().expect("a comment run is open");
                run.1.push(' ');
                run.1.push_str(body.trim());
            }
            Some(body) => {
                out.push((at + 1, body.trim().to_string()));
                in_comment = true;
            }
            None => {
                out.push((at + 1, line.to_string()));
                in_comment = false;
            }
        }
    }
    out
}

/// Comment runs and code lines under `src/mesh` that cite a transport crate version
/// (comments and assertion messages alike) cite `coyote-rns-transport` at the version
/// `Cargo.toml` pins: never the upstream name with a version, never the shortened
/// `rns-transport x.y.z` spelling a crate-name grep misses, and never a git revision or a
/// transport `release x.y.z` phrase, which is how the pre-release pin used to be cited.
/// Comment runs are folded first, so a citation wrapped across two lines is read whole,
/// and a name may carry backticks or a `v` before the version.
#[test]
fn mesh_comments_cite_the_pinned_transport_version() {
    let cargo_toml = read(repo_root().join("Cargo.toml"));
    let version = cargo_toml_dependency_version(&cargo_toml, TRANSPORT_CRATE);
    let cited = Regex::new(
        r"(?<![\w-])`?((?:coyote-rns|reticulum-rs)-transport|rns-transport)`? v?(\d+\.\d+\.\d+)",
    )
    .unwrap();
    let stale =
        Regex::new(r"\b(?:rev|commit) [0-9a-f]{7,40}\b|transport[^.]*\brelease \d+\.\d+\.\d+")
            .unwrap();
    let mut files = Vec::new();
    rust_files(&repo_root().join("src").join("mesh"), &mut files);
    let mut citations = 0;
    let mut wrong = Vec::new();
    for path in files {
        let text = read(&path);
        let file = path
            .strip_prefix(repo_root())
            .unwrap()
            .display()
            .to_string();
        for (at, run) in folded_comment_runs(&text) {
            for found in cited.captures_iter(&run) {
                let found = found.unwrap();
                citations += 1;
                let name = found.get(1).unwrap().as_str();
                let cited_version = found.get(2).unwrap().as_str();
                if name != TRANSPORT_CRATE || cited_version != version {
                    wrong.push(format!("{file}:{at}: {name} {cited_version}"));
                }
            }
            if let Some(found) = stale.find(&run).unwrap() {
                wrong.push(format!("{file}:{at}: {}", found.as_str()));
            }
        }
    }
    assert!(
        citations > 0,
        "src/mesh cites the transport version somewhere"
    );
    assert!(
        wrong.is_empty(),
        "every transport version cited under src/mesh must read `{TRANSPORT_CRATE} {version}`, with no git revision or release phrase:\n{}",
        wrong.join("\n")
    );
}

/// Every `[[package]]` block of `Cargo.lock`: name, version, the `source` line when there
/// is one (the workspace's own package has none) and the `dependencies = [...]` entries.
struct LockEntry {
    name: String,
    version: String,
    source: Option<String>,
    dependencies: Vec<String>,
}

fn lock_entries(lock: &str) -> Vec<LockEntry> {
    let field = |line: &str, key: &str| {
        line.strip_prefix(&format!("{key} = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .map(str::to_string)
    };
    let mut entries = Vec::new();
    let mut lines = lock.lines().peekable();
    while let Some(line) = lines.next() {
        if line != "[[package]]" {
            continue;
        }
        let mut name = None;
        let mut version = None;
        let mut source = None;
        let mut dependencies = Vec::new();
        while let Some(line) = lines.peek().copied() {
            if line == "[[package]]" {
                break;
            }
            lines.next();
            if let Some(found) = field(line, "name") {
                name = Some(found);
            } else if let Some(found) = field(line, "version") {
                version = Some(found);
            } else if let Some(found) = field(line, "source") {
                source = Some(found);
            } else if let Some(entry) = line
                .trim()
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix("\","))
            {
                dependencies.push(entry.to_string());
            }
        }
        entries.push(LockEntry {
            name: name.expect("every Cargo.lock package has a name"),
            version: version.expect("every Cargo.lock package has a version"),
            source,
            dependencies,
        });
    }
    entries
}

/// The one dependency line `Cargo.toml` has for `name`, whether it is written as a plain
/// `name = "x.y"` string or as an inline table carrying `version = "x.y"`.
fn cargo_toml_dependency_requirement(cargo_toml: &str, name: &str) -> String {
    let plain = format!("{name} = \"");
    let table = format!("{name} = {{");
    cargo_toml
        .lines()
        .find_map(|line| {
            if let Some(rest) = line.strip_prefix(&plain) {
                return rest.strip_suffix('"').map(str::to_string);
            }
            line.strip_prefix(&table).and_then(|rest| {
                rest.split_once("version = \"")
                    .and_then(|(_, rest)| rest.split_once('"'))
                    .map(|(version, _)| version.to_string())
            })
        })
        .unwrap_or_else(|| panic!("Cargo.toml has no dependency line for `{name}`"))
}

/// The package name `Cargo.toml` declares for this crate, the only lock entry without a
/// `source`.
fn workspace_package_name(cargo_toml: &str) -> String {
    cargo_toml
        .split_once("[package]")
        .map(|(_, rest)| rest)
        .and_then(|rest| {
            rest.lines()
                .find_map(|line| line.strip_prefix("name = \""))
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .map(str::to_string)
        .expect("Cargo.toml [package] has a name")
}

/// Patches to the transport are published in the fork crate, never applied locally, so
/// nothing in the graph may come from anywhere but crates.io: every package `Cargo.lock`
/// resolves other than this crate itself carries the crates.io registry as its source (no
/// `git+`, no path, no alternate registry), and no cargo config file or `vendor/` tree sits
/// in the checkout to redirect a source behind the lockfile's back. This is the file-level
/// twin of the CI merge gate's `cargo metadata --locked` grep for `"source":"git+"`.
#[test]
fn every_lock_entry_but_the_crate_itself_resolves_from_crates_io_and_nothing_redirects_a_source() {
    let cargo_toml = read(repo_root().join("Cargo.toml"));
    let own = workspace_package_name(&cargo_toml);
    let entries = lock_entries(&read(repo_root().join("Cargo.lock")));
    assert!(
        entries.len() > 100,
        "Cargo.lock parsed into {} packages",
        entries.len()
    );
    let elsewhere: Vec<String> = entries
        .iter()
        .filter(|entry| entry.name != own)
        .filter(|entry| entry.source.as_deref() != Some(CRATES_IO))
        .map(|entry| {
            format!(
                "{} {} <- {}",
                entry.name,
                entry.version,
                entry
                    .source
                    .as_deref()
                    .unwrap_or("(no source: path dependency)")
            )
        })
        .collect();
    assert!(
        elsewhere.is_empty(),
        "every dependency must resolve from crates.io; `cargo install coyote-ai` sees nothing else:\n{}",
        elsewhere.join("\n")
    );
    assert_eq!(
        entries.iter().filter(|entry| entry.name == own).count(),
        1,
        "Cargo.lock carries the crate itself exactly once"
    );
    assert!(
        !repo_root().join("vendor").exists(),
        "a `vendor/` tree must not exist: vendored transport patches would be invisible to `cargo install`"
    );
    for name in [".cargo/config.toml", ".cargo/config"] {
        let path = repo_root().join(name);
        if !path.exists() {
            continue;
        }
        let text = read(&path);
        for forbidden in ["[source", "[patch", "replace-with", "paths = ["] {
            assert!(
                !text.contains(forbidden),
                "{name} must not carry `{forbidden}`: a source replacement would make the lockfile lie about where the transport comes from"
            );
        }
    }
}

/// The pin comments say `rand_core` and `rmpv` "move together" with the transport. That is
/// a fact about the graph, not a sentence: the generation `Cargo.toml` pins for each is the
/// one the fork package itself depends on, both resolve to the same lock entry on the crate
/// and on the fork, and `rmpv` (whose `Value` sits in the transport's request signatures)
/// exists exactly once in the graph at the 1.3 line the fork pins.
#[test]
fn the_rand_core_and_rmpv_pins_follow_the_transports_own_generations() {
    let cargo_toml = read(repo_root().join("Cargo.toml"));
    let own = workspace_package_name(&cargo_toml);
    let entries = lock_entries(&read(repo_root().join("Cargo.lock")));
    let find = |name: &str| {
        let found: Vec<&LockEntry> = entries.iter().filter(|entry| entry.name == name).collect();
        assert_eq!(found.len(), 1, "Cargo.lock resolves `{name}` exactly once");
        found[0]
    };
    let transport = find(TRANSPORT_CRATE);
    let coyote = find(&own);

    let rand_core_req = cargo_toml_dependency_requirement(&cargo_toml, "rand_core");
    let rand_core_edge = |entry: &LockEntry| -> String {
        let edges: Vec<&String> = entry
            .dependencies
            .iter()
            .filter(|dep| *dep == "rand_core" || dep.starts_with("rand_core "))
            .collect();
        assert_eq!(
            edges.len(),
            1,
            "`{}` depends on exactly one rand_core generation: {edges:?}",
            entry.name
        );
        edges[0].clone()
    };
    let on_transport = rand_core_edge(transport);
    let on_coyote = rand_core_edge(coyote);
    assert_eq!(
        on_transport, on_coyote,
        "`{TRANSPORT_CRATE}` and `{own}` must share one rand_core entry or the transport's `CryptoRngCore` cannot be fed from here"
    );
    let resolved = on_coyote
        .strip_prefix("rand_core ")
        .map(str::to_string)
        .unwrap_or_else(|| find("rand_core").version.clone());
    assert!(
        resolved.starts_with(&format!("{rand_core_req}.")) || resolved == rand_core_req,
        "Cargo.toml pins rand_core `{rand_core_req}` but the shared lock entry is `{resolved}`"
    );

    let rmpv_req = cargo_toml_dependency_requirement(&cargo_toml, "rmpv");
    let rmpv = find("rmpv");
    assert!(
        rmpv.version.starts_with(&format!("{rmpv_req}.")),
        "the one rmpv in the graph is `{}` but Cargo.toml pins `{rmpv_req}`",
        rmpv.version
    );
    for entry in [transport, coyote] {
        assert!(
            entry.dependencies.iter().any(|dep| dep == "rmpv"),
            "`{}` must depend on the single unqualified `rmpv` entry so `Value` is one type on both sides",
            entry.name
        );
    }
}

/// Text files Coyote ships or runs under `dir`, recursively: source, docs, scripts,
/// workflows and manifests. Binary fixtures are left alone.
fn text_files(dir: &Path, into: &mut Vec<PathBuf>) {
    const TEXT: [&str; 12] = [
        "rs", "md", "sh", "ps1", "py", "yaml", "yml", "toml", "tmpl", "txt", "json", "template",
    ];
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            text_files(&path, into);
        } else if path
            .extension()
            .is_some_and(|ext| TEXT.iter().any(|t| ext == *t))
            || path.file_name().is_some_and(|name| name == "Dockerfile")
        {
            into.push(path);
        }
    }
}

/// Across everything that ships or runs (not just the five dependency documents and
/// `src/mesh`), the upstream crate is named only as the fork's origin, and the retired pin
/// is cited nowhere: no git revision, no transport version other than the one `Cargo.toml`
/// holds. A stale `0.12.0` in a README or a workflow is exactly what a bump leaves behind.
#[test]
fn no_shipped_file_names_the_upstream_transport_as_the_dependency_or_cites_the_retired_pin() {
    let cargo_toml = read(repo_root().join("Cargo.toml"));
    let version = cargo_toml_dependency_version(&cargo_toml, TRANSPORT_CRATE);
    let cited = Regex::new(
        r"(?<![\w-])`?((?:coyote-rns|reticulum-rs)-transport|rns-transport)`? v?(\d+\.\d+\.\d+)",
    )
    .unwrap();
    let git_rev = Regex::new(r"\b(?:rev|commit) [0-9a-f]{7,40}\b").unwrap();
    let mut files = vec![
        repo_root().join("README.md"),
        repo_root().join("Cargo.toml"),
        repo_root().join("Dockerfile"),
        repo_root().join("config.example.yaml"),
    ];
    for dir in ["src", "tests", "scripts", "deployment", ".github", "assets"] {
        text_files(&repo_root().join(dir), &mut files);
    }
    let mut wrong = Vec::new();
    let mut upstream_mentions = 0;
    for path in &files {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let file = path
            .strip_prefix(repo_root())
            .unwrap()
            .display()
            .to_string();
        for paragraph in text.split("\n\n") {
            if paragraph.contains(UPSTREAM_TRANSPORT_CRATE) {
                upstream_mentions += 1;
                if !paragraph.contains("fork") {
                    wrong.push(format!(
                        "{file}: names {UPSTREAM_TRANSPORT_CRATE} without calling it the fork's origin:\n{}",
                        paragraph.trim()
                    ));
                }
            }
        }
        for (at, run) in folded_comment_runs(&text) {
            for found in cited.captures_iter(&run) {
                let found = found.unwrap();
                let name = found.get(1).unwrap().as_str();
                let cited_version = found.get(2).unwrap().as_str();
                if name != TRANSPORT_CRATE || cited_version != version {
                    wrong.push(format!("{file}:{at}: cites {name} {cited_version}"));
                }
            }
            if run.contains("transport")
                && let Some(found) = git_rev.find(&run).unwrap()
            {
                wrong.push(format!(
                    "{file}:{at}: cites a transport git revision: {}",
                    found.as_str()
                ));
            }
        }
    }
    assert!(
        upstream_mentions > 0,
        "the sweep saw the fork-origin paragraphs, so it is reading the right files"
    );
    assert!(
        wrong.is_empty(),
        "the upstream transport may be named only as the fork's origin and the transport version only as `{TRANSPORT_CRATE} {version}`:\n{}",
        wrong.join("\n")
    );
}

/// The interop verification record says which host it was taken on and keeps the Python
/// reference pinned where the harness pins it: the two commits in the README's table are
/// the ones `setup.sh` checks out and `Reference::spawn` re-checks, and the RNS release
/// named in the record is the one the propagation-node image installs.
#[test]
fn the_interop_record_names_its_host_and_its_python_pins_match_the_harness() {
    let readme = read(scripts_dir().join("mesh-interop").join("README.md"));
    let record = readme
        .split_once("### Verification record")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("\n## ").map(|(record, _)| record))
        .expect("scripts/mesh-interop/README.md has a `### Verification record` section");
    let host = Regex::new(r"Last verified \d{4}-\d{2}-\d{2}, on (Linux|macOS|Windows)\b").unwrap();
    assert!(
        host.is_match(record).unwrap(),
        "the verification record must open with `Last verified <date>, on <Linux|macOS|Windows>`"
    );
    let rns = dockerfile_rns_version();
    assert!(
        record.contains(&format!("RNS `{rns}`")),
        "the record must name the Python side at RNS `{rns}`, the release the propagation-node image installs"
    );
    let table_pin = |repo: &str| -> String {
        let row = Regex::new(&format!(
            r"\| https://github\.com/markqvist/{repo} \| `([0-9a-f]{{40}})` \|"
        ))
        .unwrap();
        row.captures(&readme)
            .unwrap()
            .unwrap_or_else(|| {
                panic!("the README pins table has a {repo} row with a 40-hex commit")
            })
            .get(1)
            .unwrap()
            .as_str()
            .to_string()
    };
    let setup = read(scripts_dir().join("mesh-interop").join("setup.sh"));
    let harness = read(
        repo_root()
            .join("src")
            .join("mesh")
            .join("conformance")
            .join("interop.rs"),
    );
    for (repo, var) in [("Reticulum", "RETICULUM_PIN"), ("LXMF", "LXMF_PIN")] {
        let pin = table_pin(repo);
        assert!(
            setup.contains(&format!("{var}=\"{pin}\"")),
            "setup.sh must pin {repo} at the README's `{pin}`"
        );
        assert!(
            harness.contains(&format!("const {var}: &str = \"{pin}\";")),
            "src/mesh/conformance/interop.rs must re-check {repo} at the README's `{pin}`"
        );
    }
}
