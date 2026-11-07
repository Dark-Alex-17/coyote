//! Cold-start, real-binary contract for sbx kit discovery and the
//! corrupted-cache rule of the embedded base kit.
//!
//! The base-kit half runs config-less: `--sandbox` extracts (or heals) the
//! kit under `<cache>/sbx-kits/coyote/` before it checks for a host config,
//! so a seeded cache can be corrupted on purpose and inspected after the
//! refusal. The discovery half runs with a dry-run config and a cooperative
//! `sbx` stub that answers `version` with a current release, `secret ls` /
//! `ls` with nothing registered, and records every other call, so the kits
//! the binary discovers show up in the argv it hands to `sbx create`.
//!
//! Every run uses a fresh fake HOME, config dir and cache dir so host state
//! cannot leak in; the roots are canonicalized because discovery reports the
//! resolved current dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

const KIT_YAML: &str = "coyote.yaml";
const KIT_DOCKERFILE: &str = "coyote.dockerfile";
const KITS_DIR: &str = "sbx-kits";
const HANDLE: &str = "coyote";
const STRAY_FILE: &str = "notes.yaml";
const HOST_SBX_VERSION_LINE: &str = "sbx version v0.49.1 c5b19951af121b439800029695f88bf1139a0b0b";

const VALID_MIXIN: &str = r#"# syntax=docker/sandbox-kit:3
schemaVersion: "3"
kind: mixin
description: fixture
capabilities:
  - type: com.docker.sandbox/network-policy@1
    config:
      runtime:
        allow: [api.example.com, pypi.org]
      install:
        allow: [deb.debian.org]
  - type: com.docker.sandbox/lifecycle@1
    config:
      install:
        - command: echo one
"#;

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_root(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-sbx-discovery-{label}-{}-{unique}",
        process::id()
    ));
    for sub in ["home", "config", "cache", "bin"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    dir.canonicalize().unwrap()
}

fn repo_asset(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/sbx-kit")
        .join(HANDLE)
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn write_dry_run_config(root: &Path) {
    let vault_pass = root.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        root.join("config/config.yaml"),
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
             stream: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

/// An `sbx` that records every call, reports a current version, no
/// registered secrets and no existing sandboxes, and succeeds at everything
/// else.
fn install_stub_sbx(root: &Path) -> PathBuf {
    install_stub_sbx_with(root, HOST_SBX_VERSION_LINE, &[])
}

/// Like [`install_stub_sbx`], but `version` prints `version_line` and `ls`
/// lists `sandbox_rows` under the `NAME STATUS` header.
fn install_stub_sbx_with(root: &Path, version_line: &str, sandbox_rows: &[&str]) -> PathBuf {
    let log = root.join("stub-sbx-calls");
    let stub = root.join("bin/sbx");
    let ls_lines = std::iter::once("NAME STATUS")
        .chain(sandbox_rows.iter().copied())
        .map(|row| format!("printf '%s\\n' '{row}'; "))
        .collect::<String>();
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = \"version\" ]; then printf '%s\\n' '{version_line}'; exit 0; fi\n\
             if [ \"$1\" = \"secret\" ] && [ \"$2\" = \"ls\" ]; then exit 0; fi\n\
             if [ \"$1\" = \"ls\" ]; then {ls_lines}exit 0; fi\n\
             exit 0\n",
            log = log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    log
}

/// Removes every inherited `COYOTE_*` variable, so a developer's own
/// override (`COYOTE_CONFIG_FILE` beats the scratch config dir) never
/// reaches the child; the harness then sets the ones it needs.
fn scrub_coyote_env(cmd: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("COYOTE_")) {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn run_coyote_in(root: &Path, cwd: &Path, args: &[&str], extra_env: &[(&str, &Path)]) -> Output {
    let home = root.join("home");
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_coyote"));
    scrub_coyote_env(&mut command)
        .args(args)
        .current_dir(cwd)
        .env("COYOTE_CONFIG_DIR", root.join("config"))
        .env("COYOTE_CACHE_DIR", root.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PATH", path)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_SANDBOX_KIT")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .stdin(Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn run_coyote(root: &Path, args: &[&str]) -> Output {
    run_coyote_in(root, root, args, &[])
}

fn run_coyote_with_kit_override(root: &Path, kit: &Path) -> Output {
    run_coyote_in(
        root,
        root,
        &["--sandbox", "probe"],
        &[("COYOTE_SANDBOX_KIT", kit)],
    )
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn kits_root(root: &Path) -> PathBuf {
    root.join("cache").join(KITS_DIR)
}

fn kit_dir(root: &Path) -> PathBuf {
    kits_root(root).join(HANDLE)
}

fn hash_file(root: &Path) -> PathBuf {
    kits_root(root).join(format!("{HANDLE}.sha256"))
}

fn stub_calls(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Every regular file under the kits cache root, relative to it.
fn cache_listing(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path.strip_prefix(base).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(&kits_root(root), &kits_root(root), &mut out);
    out.sort();
    out
}

fn assert_kit_matches_assets(label: &str, root: &Path) {
    for name in [KIT_YAML, KIT_DOCKERFILE] {
        let path = kit_dir(root).join(name);
        let on_disk =
            fs::read(&path).unwrap_or_else(|e| panic!("{label}: read {}: {e}", path.display()));
        assert_eq!(
            on_disk,
            repo_asset(name),
            "{label}: {name} differs from the asset"
        );
    }
}

fn read_hash(root: &Path) -> String {
    fs::read_to_string(hash_file(root))
        .unwrap()
        .trim()
        .to_string()
}

/// Runs the config-less `--sandbox` path and asserts it bailed on the
/// missing host config after asking the stub for nothing but its version.
/// The stub log is reset first so repeated runs are judged one at a time.
fn sandbox_bails_on_missing_config(label: &str, root: &Path, log: &Path) {
    let _ = fs::remove_file(log);
    let output = run_coyote(root, &["--sandbox", "probe"]);
    assert!(!output.status.success(), "{label}: expected a refusal");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("No Coyote config found"),
        "{label}: unexpected stderr:\n{stderr}"
    );
    assert_only_version_probed(label, log);
}

fn assert_only_version_probed(label: &str, log: &Path) {
    assert_eq!(
        stub_calls(log),
        ["version"],
        "{label}: the stub sbx should have been asked for its version and nothing else"
    );
}

fn write_kit(kits_dir: &Path, handle: &str, yaml: &str) -> PathBuf {
    let dir = kits_dir.join(handle);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{handle}.yaml")), yaml).unwrap();
    dir
}

/// Every value that follows a `--kit` token in the recorded `create` call.
fn create_kit_args(calls: &[String]) -> Vec<String> {
    let create = calls
        .iter()
        .find(|line| line.starts_with("create "))
        .unwrap_or_else(|| panic!("no create call recorded in {calls:?}"));
    let words: Vec<&str> = create.split(' ').collect();
    words
        .windows(2)
        .filter(|pair| pair[0] == "--kit")
        .map(|pair| pair[1].to_string())
        .collect()
}

#[test]
fn usage_probe_sandbox_heals_a_corrupted_cached_kit_yaml_despite_a_matching_hash() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("heal-yaml");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);

    sandbox_bails_on_missing_config("seed run", &root, &log);
    let hash = read_hash(&root);
    assert_kit_matches_assets("seed run", &root);

    // Corrupt the yaml but leave the hash file matching: the old rule
    // ("hash matches, nothing to do") would keep the junk forever.
    let yaml = kit_dir(&root).join(KIT_YAML);
    fs::write(&yaml, "not: [a kit\n").unwrap();
    // A stray sibling inside the kit dir goes with the rewrite as well.
    fs::write(kit_dir(&root).join("notes.txt"), "scratch\n").unwrap();

    sandbox_bails_on_missing_config("heal run", &root, &log);

    assert_kit_matches_assets("heal run", &root);
    assert_eq!(read_hash(&root), hash, "the hash content is unchanged");
    assert_eq!(
        cache_listing(&root),
        vec![
            format!("{HANDLE}.sha256"),
            format!("{HANDLE}/{KIT_DOCKERFILE}"),
            format!("{HANDLE}/{KIT_YAML}"),
        ],
        "the heal rewrites the kit dir wholesale"
    );

    // A truncated dockerfile is healed the same way: the workload check
    // needs both files, not just a parseable yaml.
    fs::remove_file(kit_dir(&root).join(KIT_DOCKERFILE)).unwrap();
    sandbox_bails_on_missing_config("heal dockerfile run", &root, &log);
    assert_kit_matches_assets("heal dockerfile run", &root);
    assert_eq!(read_hash(&root), hash);
}

#[test]
fn usage_probe_sandbox_heals_a_regular_file_squatting_on_the_kit_dir_path() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("heal-file");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);

    sandbox_bails_on_missing_config("seed run", &root, &log);
    let hash = read_hash(&root);

    // Replace the kit dir with a regular file of the same name while the
    // hash still matches.
    fs::remove_dir_all(kit_dir(&root)).unwrap();
    fs::write(kit_dir(&root), "I am not a directory\n").unwrap();
    assert!(kit_dir(&root).is_file());

    sandbox_bails_on_missing_config("heal run", &root, &log);

    assert!(
        kit_dir(&root).is_dir(),
        "the squatting file was not replaced"
    );
    assert_kit_matches_assets("heal run", &root);
    assert_eq!(read_hash(&root), hash);
    assert_eq!(
        cache_listing(&root),
        vec![
            format!("{HANDLE}.sha256"),
            format!("{HANDLE}/{KIT_DOCKERFILE}"),
            format!("{HANDLE}/{KIT_YAML}"),
        ]
    );
}

#[test]
fn usage_probe_sandbox_refuses_a_malformed_kit_before_creating_a_sandbox() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("bad-kit");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);
    let good = write_kit(&root.join("config").join("kits"), "good", VALID_MIXIN);
    // Stem differs from the dir name: `kit::load` refuses this dir.
    let bad = root.join("config").join("kits").join("bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("other.yaml"), VALID_MIXIN).unwrap();

    let output = run_coyote(&root, &["--sandbox", "probe", "--fresh"]);

    assert!(!output.status.success(), "expected a refusal");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&bad.display().to_string()),
        "the refusal must name the offending kit dir:\n{stderr}"
    );
    assert!(
        !stderr.contains(&good.display().to_string()),
        "the valid sibling is not blamed:\n{stderr}"
    );
    let calls = stub_calls(&log);
    assert!(
        calls.iter().any(|call| call == "ls"),
        "discovery runs after sbx has been asked for its sandboxes: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|call| !call.starts_with("create ") && !call.starts_with("run ")),
        "the refusal must land before any sandbox is created or run: {calls:?}"
    );
    // The base kit was still extracted: discovery runs after the kit cache
    // is settled.
    assert_kit_matches_assets("bad kit run", &root);
}

#[test]
fn usage_probe_sandbox_passes_discovered_kits_to_create_in_root_order_and_ignores_stray_yaml() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("discovery");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);
    let config = root.join("config");

    // Roots seeded in reverse of the documented order so the order in the
    // argv is the binary's doing, not the filesystem's.
    let workspace = root.join("ws");
    let workspace_kits = workspace.join(".coyote").join("kits");
    let w = write_kit(&workspace_kits, "w", VALID_MIXIN);
    let h = write_kit(&config.join("hooks").join("kits"), "h", VALID_MIXIN);
    let rh = write_kit(
        &config.join("roles").join("hooks").join("kits"),
        "rh",
        VALID_MIXIN,
    );
    let ro = write_kit(&config.join("roles").join("kits"), "ro", VALID_MIXIN);
    let r = write_kit(&config.join("rags").join("kits"), "r", VALID_MIXIN);
    let researcher = config.join("agents").join("researcher");
    let ah = write_kit(&researcher.join("hooks").join("kits"), "ah", VALID_MIXIN);
    let a = write_kit(&researcher.join("kits"), "a", VALID_MIXIN);
    let f = write_kit(&config.join("functions").join("kits"), "f", VALID_MIXIN);
    let c = write_kit(&config.join("kits"), "c", VALID_MIXIN);
    let cwd = workspace.join("x").join("y");
    fs::create_dir_all(&cwd).unwrap();

    // A yaml file that is not inside a `kits/<handle>/` dir is not a kit,
    // wherever it sits: ignored, never discovered, never warned about.
    let stray_yaml = "title: stray\nkind: mixin\n";
    for path in [
        config.join(STRAY_FILE),
        config.join("functions").join(STRAY_FILE),
        config.join("functions").join("x").join(STRAY_FILE),
        config.join("agents").join("researcher").join(STRAY_FILE),
        config
            .join("agents")
            .join("researcher")
            .join(format!("docs.{STRAY_FILE}")),
        config.join("hooks").join(format!("h.{STRAY_FILE}")),
        workspace.join(".coyote").join(STRAY_FILE),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, stray_yaml).unwrap();
    }
    // A stray file beside the kit dirs is ignored too.
    fs::write(config.join("kits").join("README.md"), "about\n").unwrap();

    let output = run_coyote_in(&root, &cwd, &["--sandbox", "probe", "--fresh"], &[]);

    let stderr = stderr_of(&output);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "expected the stubbed launch to succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let calls = stub_calls(&log);
    let create = calls
        .iter()
        .find(|call| call.starts_with("create "))
        .unwrap_or_else(|| panic!("no create call recorded in {calls:?}"));
    assert!(
        create.starts_with(&format!(
            "create --name probe {} --kit ",
            kit_dir(&root).display()
        )),
        "the base kit is the positional workload kit: {create}"
    );
    assert!(create.ends_with(" ."), "{create}");
    assert_eq!(
        create_kit_args(&calls),
        vec![
            c.display().to_string(),
            f.display().to_string(),
            a.display().to_string(),
            ah.display().to_string(),
            r.display().to_string(),
            ro.display().to_string(),
            rh.display().to_string(),
            h.display().to_string(),
            w.display().to_string(),
        ],
        "every discovered kit in root order, the base kit never among the --kit values: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call == "run --name probe"),
        "{calls:?}"
    );

    let lines: Vec<&str> = stdout.lines().collect();
    let header = lines
        .iter()
        .position(|line| *line == "Applying 9 sbx kit(s):")
        .unwrap_or_else(|| panic!("no discovery header in stdout:\n{stdout}"));
    let counts = "(adds: 1 install hook(s), 2 runtime domain(s), 1 install domain(s))";
    let expected = [
        format!("  config {counts}"),
        format!("  functions {counts}"),
        format!("  agents/researcher {counts}"),
        format!("  agents/researcher/hooks {counts}"),
        format!("  rags {counts}"),
        format!("  roles {counts}"),
        format!("  roles/hooks {counts}"),
        format!("  hooks {counts}"),
        format!("  workspace {} {counts}", workspace_kits.display()),
    ];
    assert_eq!(
        &lines[header + 1..header + 1 + expected.len()],
        &expected[..],
        "{stdout}"
    );

    // No surface mentions the stray files: not stderr, not the log file.
    let log_text = fs::read_to_string(root.join("cache").join("coyote.log")).unwrap_or_default();
    for (surface, text) in [("stderr", &stderr), ("stdout", &stdout), ("log", &log_text)] {
        assert!(
            !text.contains(STRAY_FILE) && !text.contains("stray"),
            "{surface} mentions the ignored stray files:\n{text}"
        );
    }
    // Only the base kit lives in the cache: with `--fresh` no credentials
    // kit is generated, and discovered kits are passed in place.
    assert_eq!(
        cache_listing(&root),
        vec![
            format!("{HANDLE}.sha256"),
            format!("{HANDLE}/{KIT_DOCKERFILE}"),
            format!("{HANDLE}/{KIT_YAML}"),
        ]
    );
}

#[test]
fn sandbox_reattach_runs_without_discovery_when_the_sandbox_exists() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("reattach");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx_with(&root, HOST_SBX_VERSION_LINE, &["probe running"]);
    write_dry_run_config(&root);
    // Discovery would refuse this kit dir, so it must never be looked at
    // when the sandbox already exists.
    let bad = root.join("config").join("kits").join("bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("other.yaml"), VALID_MIXIN).unwrap();

    let output = run_coyote(&root, &["--sandbox", "probe", "--fresh"]);

    let stderr = stderr_of(&output);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "expected the re-attach to succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let calls = stub_calls(&log);
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == "run --name probe")
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        calls.iter().all(|call| !call.starts_with("create")),
        "{calls:?}"
    );
    assert!(
        !stderr.contains(&bad.display().to_string()),
        "the malformed kit was inspected:\n{stderr}"
    );
    assert!(!stdout.contains("Applying"), "{stdout}");
}

#[test]
fn sandbox_bails_on_an_old_sbx_before_listing_sandboxes() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("old-sbx");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx_with(&root, "sbx version: v0.44.9 deadbeef", &[]);
    write_dry_run_config(&root);

    let output = run_coyote(&root, &["--sandbox", "probe"]);

    assert!(!output.status.success(), "expected a refusal");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(
            "sbx v0.44.9 is too old: Coyote's sandbox kits use the v3 kit format, which needs \
             sbx v0.45.0 or newer (https://docs.docker.com/ai/sandboxes/get-started/)"
        ),
        "{stderr}"
    );
    assert_only_version_probed("old sbx", &log);
}

#[test]
fn sandbox_kit_override_at_an_empty_dir_is_refused_before_asking_sbx() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("override-empty");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);
    let empty = root.join("empty");
    fs::create_dir_all(&empty).unwrap();

    let output = run_coyote_with_kit_override(&root, &empty);

    assert!(!output.status.success(), "expected a refusal");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "kit dir '{}' must hold exactly one 'empty.yaml' (found: none)",
            empty.display()
        )),
        "{stderr}"
    );
    assert_only_version_probed("empty override", &log);
}

#[test]
fn sandbox_kit_override_at_a_mixin_is_refused_before_asking_sbx() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("override-mixin");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);
    let mixin = write_kit(&root, "m", VALID_MIXIN);

    let output = run_coyote_with_kit_override(&root, &mixin);

    assert!(!output.status.success(), "expected a refusal");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("must have kind 'workload'"), "{stderr}");
    assert_only_version_probed("mixin override", &log);
}

#[test]
fn sandbox_kit_override_without_a_dockerfile_is_refused_before_asking_sbx() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("override-no-dockerfile");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);
    let workload = root.join("w");
    fs::create_dir_all(&workload).unwrap();
    fs::write(workload.join("w.yaml"), repo_asset(KIT_YAML)).unwrap();

    let output = run_coyote_with_kit_override(&root, &workload);

    assert!(!output.status.success(), "expected a refusal");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("is missing 'w.dockerfile'"), "{stderr}");
    assert_only_version_probed("dockerfile-less override", &log);
}

/// A valid workload override is handed to `sbx create` as the positional
/// kit in place of the cached base kit, which is then never extracted.
#[test]
fn usage_probe_sandbox_kit_override_at_a_workload_kit_is_the_positional_create_argument() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("override-workload");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);
    let workload = root.join("w");
    fs::create_dir_all(&workload).unwrap();
    fs::write(workload.join("w.yaml"), repo_asset(KIT_YAML)).unwrap();
    fs::write(workload.join("w.dockerfile"), repo_asset(KIT_DOCKERFILE)).unwrap();
    // One discoverable mixin so the override is visibly the positional
    // argument and not just another `--kit` value.
    let c = write_kit(&root.join("config").join("kits"), "c", VALID_MIXIN);

    let output = run_coyote_in(
        &root,
        &root,
        &["--sandbox", "probe", "--fresh"],
        &[("COYOTE_SANDBOX_KIT", &workload)],
    );

    let stderr = stderr_of(&output);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "expected the stubbed launch to succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let calls = stub_calls(&log);
    let create = calls
        .iter()
        .find(|call| call.starts_with("create "))
        .unwrap_or_else(|| panic!("no create call recorded in {calls:?}"));
    assert_eq!(
        *create,
        format!(
            "create --name probe {} --kit {} .",
            workload.display(),
            c.display()
        ),
        "{calls:?}"
    );
    assert!(
        create.split(' ').all(|word| word != HANDLE),
        "the agent token must not be an argv element: {create}"
    );
    assert!(
        calls.iter().any(|call| call == "run --name probe"),
        "{calls:?}"
    );
    assert!(
        !kits_root(&root).exists(),
        "a valid override must not extract the embedded kit: {:?}",
        cache_listing(&root)
    );
}

/// Rewrites the stub so `sbx version` prints `version_line` but exits with
/// `code`, leaving every other call untouched.
fn make_stub_version_fail(root: &Path, version_line: &str, code: u8) {
    let stub = root.join("bin/sbx");
    let script = fs::read_to_string(&stub).unwrap();
    let ok = format!("printf '%s\\n' '{version_line}'; exit 0; fi");
    let failing = format!("printf '%s\\n' '{version_line}'; exit {code}; fi");
    assert!(script.contains(&ok), "stub shape changed:\n{script}");
    fs::write(&stub, script.replacen(&ok, &failing, 1)).unwrap();
}

/// A `sbx version` that exits non-zero is only warned about, even when its
/// output names a release older than the minimum: a failing probe is not a
/// verdict, so the launch goes on to list and create.
#[test]
fn usage_probe_sandbox_proceeds_when_sbx_version_exits_non_zero_despite_an_old_token() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("failing-version");
    let _cleanup = TempDirGuard(root.clone());
    let old = "sbx version: v0.10.0 deadbeef";
    let log = install_stub_sbx_with(&root, old, &[]);
    make_stub_version_fail(&root, old, 2);
    write_dry_run_config(&root);

    let output = run_coyote(&root, &["--sandbox", "probe", "--fresh"]);

    let stderr = stderr_of(&output);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "a failing version probe must not refuse the launch\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!stderr.contains("is too old"), "{stderr}");
    let calls = stub_calls(&log);
    assert_eq!(
        calls.first().map(String::as_str),
        Some("version"),
        "{calls:?}"
    );
    assert!(calls.iter().any(|call| call == "ls"), "{calls:?}");
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("create --name probe ")),
        "{calls:?}"
    );
    assert_eq!(
        calls.last().map(String::as_str),
        Some("run --name probe"),
        "{calls:?}"
    );
    let log_text = fs::read_to_string(root.join("cache").join("coyote.log")).unwrap_or_default();
    assert!(
        log_text.contains("[WARN]") && log_text.contains("`sbx version` exited with an error"),
        "the failing probe is warned about, not swallowed:\n{log_text}"
    );
    assert!(
        !log_text.contains("Could not determine the sbx version"),
        "{log_text}"
    );
}

/// An `sbx` that `which` finds but that cannot be spawned (here: a script
/// whose interpreter does not exist) is warned about and the launch goes
/// on to the next gate, which on a config-less host is the config check.
/// PATH holds only the stub dir: a PATH search skips an entry whose
/// interpreter is missing and would otherwise fall through to a real
/// `sbx` further down the host PATH.
#[test]
fn usage_probe_sandbox_proceeds_when_sbx_version_cannot_be_spawned() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("unspawnable-sbx");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    let stub = root.join("bin/sbx");
    fs::write(&stub, "#!/nonexistent/interpreter\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // No config written on purpose: the config check is the next gate.

    let stub_only_path = root.join("bin");
    let output = run_coyote_in(
        &root,
        &root,
        &["--sandbox", "probe"],
        &[("PATH", &stub_only_path)],
    );

    assert!(!output.status.success(), "expected a refusal");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("No Coyote config found"),
        "the launch must reach the config check:\n{stderr}"
    );
    assert!(!stderr.contains("is too old"), "{stderr}");
    assert!(
        !log.exists(),
        "the stub never ran, so nothing can be logged: {:?}",
        stub_calls(&log)
    );
    let log_text = fs::read_to_string(root.join("cache").join("coyote.log")).unwrap_or_default();
    assert!(
        log_text.contains("[WARN]") && log_text.contains("Could not run `sbx version`"),
        "the failed spawn is warned about, not swallowed:\n{log_text}"
    );
    assert!(!log_text.contains("exited with an error"), "{log_text}");
    // The base kit is still extracted on the way to the config check.
    assert_kit_matches_assets("unspawnable sbx", &root);
}

/// The plain first launch: no `--fresh`, nothing discoverable, no MCP
/// config. The sandbox is created from the bare base kit as the positional
/// argument, host files are copied in, and the run is the bare `run`.
#[test]
fn usage_probe_sandbox_first_launch_without_fresh_creates_from_the_bare_base_kit() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("first-launch");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);

    let output = run_coyote(&root, &["--sandbox", "probe"]);

    let stderr = stderr_of(&output);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "expected the stubbed first launch to succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let calls = stub_calls(&log);
    assert_eq!(
        calls.first().map(String::as_str),
        Some("version"),
        "{calls:?}"
    );
    let creates: Vec<&String> = calls
        .iter()
        .filter(|call| call.starts_with("create "))
        .collect();
    assert_eq!(
        creates,
        vec![&format!(
            "create --name probe {} .",
            kit_dir(&root).display()
        )],
        "the bare base kit is the only positional and there is no --kit: {calls:?}"
    );
    let ls_at = calls.iter().position(|call| call == "ls").unwrap();
    let create_at = calls
        .iter()
        .position(|call| call.starts_with("create "))
        .unwrap();
    assert!(ls_at < create_at, "{calls:?}");
    // Without `--fresh` the host config is copied into the new sandbox.
    let config_copy = format!(
        "cp {} probe:/home/agent/.config/coyote/",
        root.join("config").join("config.yaml").display()
    );
    assert!(
        calls.contains(&config_copy),
        "the host config must be copied in on a first launch: {calls:?}"
    );
    assert_eq!(
        calls.last().map(String::as_str),
        Some("run --name probe"),
        "{calls:?}"
    );
    assert!(
        calls.iter().filter(|call| call.starts_with("run ")).count() == 1,
        "{calls:?}"
    );
    assert!(!stdout.contains("Applying"), "{stdout}");
    assert_kit_matches_assets("first launch", &root);
}

/// Without `--fresh`, re-attaching an existing sandbox is the common path:
/// it still skips discovery and ends in a bare `run --name <n>`.
#[test]
fn usage_probe_sandbox_reattach_without_fresh_skips_discovery_and_create() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("reattach-plain");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx_with(
        &root,
        HOST_SBX_VERSION_LINE,
        &["other running", "probe stopped"],
    );
    write_dry_run_config(&root);
    // Two shapes discovery would refuse: a dir whose yaml is misnamed and a
    // dir squatting on the reserved base-kit handle.
    let kits = root.join("config").join("kits");
    let bad = kits.join("bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("other.yaml"), VALID_MIXIN).unwrap();
    let reserved = write_kit(&kits, HANDLE, VALID_MIXIN);

    let output = run_coyote(&root, &["--sandbox", "probe"]);

    let stderr = stderr_of(&output);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "expected the re-attach to succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let calls = stub_calls(&log);
    assert_eq!(
        calls.last().map(String::as_str),
        Some("run --name probe"),
        "re-attach must end in the bare run: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|call| !call.starts_with("create") && !call.starts_with("run --name probe ")),
        "{calls:?}"
    );
    for dir in [&bad, &reserved] {
        assert!(
            !stderr.contains(&dir.display().to_string())
                && !stdout.contains(&dir.display().to_string()),
            "a kit dir was inspected on re-attach:\n{stdout}\n{stderr}"
        );
    }
}

/// The minimum release passes, in the older colon-separated output shape,
/// and the launch goes on to list and create.
#[test]
fn usage_probe_sandbox_accepts_the_minimum_sbx_in_the_older_version_shape() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("min-sbx");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx_with(&root, "sbx version: v0.45.0 991967d", &[]);
    write_dry_run_config(&root);

    let output = run_coyote(&root, &["--sandbox", "probe", "--fresh"]);

    let stderr = stderr_of(&output);
    assert!(
        output.status.success(),
        "v0.45.0 is the minimum and must pass:\n{stderr}"
    );
    assert!(!stderr.contains("is too old"), "{stderr}");
    let calls = stub_calls(&log);
    assert!(calls.iter().any(|call| call == "ls"), "{calls:?}");
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("create --name probe ")),
        "{calls:?}"
    );
}

/// An `sbx version` whose output carries no version token is let through:
/// the launch proceeds to list sandboxes instead of refusing.
#[test]
fn usage_probe_sandbox_proceeds_when_the_sbx_version_is_unreadable() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("odd-sbx");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx_with(&root, "sbx version: dev build 2026-10-20", &[]);
    write_dry_run_config(&root);

    let output = run_coyote(&root, &["--sandbox", "probe", "--fresh"]);

    let stderr = stderr_of(&output);
    assert!(
        output.status.success(),
        "an unreadable version must not refuse the launch:\n{stderr}"
    );
    assert!(!stderr.contains("is too old"), "{stderr}");
    let calls = stub_calls(&log);
    assert_eq!(
        calls.first().map(String::as_str),
        Some("version"),
        "{calls:?}"
    );
    assert!(calls.iter().any(|call| call == "ls"), "{calls:?}");
    assert!(
        calls.iter().any(|call| call == "run --name probe"),
        "{calls:?}"
    );
}

/// The version gate is the first thing `--sandbox` does: an old sbx is
/// refused before the host config is even looked for.
#[test]
fn usage_probe_sandbox_refuses_an_old_sbx_before_looking_for_a_config() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("old-sbx-no-config");
    let _cleanup = TempDirGuard(root.clone());
    let log = install_stub_sbx_with(&root, "sbx version v0.44.9 deadbeef", &[]);
    // No config written on purpose.

    let output = run_coyote(&root, &["--sandbox", "probe"]);

    assert!(!output.status.success(), "expected a refusal");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("sbx v0.44.9 is too old"), "{stderr}");
    assert!(
        !stderr.contains("No Coyote config found"),
        "the version gate must come before the config check:\n{stderr}"
    );
    assert_eq!(stub_calls(&log), vec!["version".to_string()]);
    assert!(
        !kits_root(&root).exists(),
        "nothing is extracted behind a refused sbx: {:?}",
        cache_listing(&root)
    );
}
