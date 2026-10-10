//! Static pins for the mesh relay setup scripts (`scripts/mesh-relay.sh`,
//! `scripts/mesh-relay.ps1`), the installer hooks that hand off to them, and the
//! Docker image's rnsd bundle (`Dockerfile`, `scripts/docker-entrypoint.sh`,
//! `scripts/reticulum.config.tmpl`, `scripts/image-smoke.sh`).
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

/// The install ladder of the Windows script: present → uv → pipx → `py -3` → `python`,
/// each python probed against the 3.9 floor, exit 2 when every rung is missing; and the
/// venv lands under COYOTE_CONFIG_DIR, else XDG_CONFIG_HOME, else %APPDATA%\coyote. CI
/// strips uv and pipx to reach `py -3`; the `python` rung and the exit 2 behind it are
/// held here by position.
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

/// The one lane that runs the scripts themselves: both linters pinned, then the relay
/// smoke on all three runner families under a throwaway HOME with every tool directory
/// pointed under it, macOS on its own /bin/bash 3.2 and Windows under both PowerShells.
/// Service installation stays out of CI on purpose and the job says so.
#[test]
fn usage_probe_the_ci_scripts_job_lints_and_smokes_the_relay_on_every_runner_family() {
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

    // The smoke matrix the plan names, in both shells.
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
        "a GHA layer cache of the ~5.7 GB image would evict the rust-cache entries the `All` matrix depends on"
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
        .find("scripts/image-smoke.sh coyote-smoke:release")
        .expect("publish-sandbox-image smokes `coyote-smoke:release`");
    let push_at = release
        .find("platforms: linux/amd64,linux/arm64")
        .expect("publish-sandbox-image still has the multi-platform push");
    assert!(
        smoke_at < push_at,
        "the smoke must precede the multi-platform push: it is the gate, and a smoke after the push guards nothing"
    );
    let before_smoke = &release[..smoke_at];
    for needle in ["load: true", "push: false", "tags: coyote-smoke:release"] {
        assert!(
            before_smoke.contains(needle),
            "the smoked image is built with `{needle}` before the smoke: loaded into the daemon, never pushed:\n{before_smoke}"
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
fn usage_probe_relay_scripts_are_config_twins_and_copy_the_propagation_node_comment() {
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

/// Every download the installers make is HTTPS over TLS 1.2 or newer and stays HTTPS
/// across redirects (curl refuses an https-to-http hop, the PowerShell installer refuses a
/// non-https asset URL, wget gets whichever TLS flags it understands), and the relay
/// script they fall back to is fetched from the repository over HTTPS. The bash prompt
/// gate reads both a TTY stdin and stdout so `curl | bash` never blocks.
#[test]
fn usage_probe_installers_pin_https_tls12_and_gate_the_prompt_on_a_tty() {
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
