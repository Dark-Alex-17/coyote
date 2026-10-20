//! Cold-start, real-binary contract for sbx kit discovery and the
//! corrupted-cache rule of the embedded base kit.
//!
//! The base-kit half runs config-less: `--sandbox` extracts (or heals) the
//! kit under `<cache>/sbx-kits/coyote/` before it checks for a host config,
//! so a seeded cache can be corrupted on purpose and inspected after the
//! refusal. The discovery half runs with a dry-run config and a cooperative
//! `sbx` stub that answers `secret ls` / `ls` with nothing registered and
//! records every other call, so the kits the binary discovers show up in the
//! argv it hands to `sbx create`.
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
const LEGACY_FILE: &str = "sbx-mixin.yaml";

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

/// An `sbx` that records every call, reports no registered secrets and no
/// existing sandboxes, and succeeds at everything else.
fn install_stub_sbx(root: &Path) -> PathBuf {
    let log = root.join("stub-sbx-calls");
    let stub = root.join("bin/sbx");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = \"secret\" ] && [ \"$2\" = \"ls\" ]; then exit 0; fi\n\
             if [ \"$1\" = \"ls\" ]; then printf 'NAME STATUS\\n'; exit 0; fi\n\
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

fn run_coyote_in(root: &Path, cwd: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_coyote"))
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
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn run_coyote(root: &Path, args: &[&str]) -> Output {
    run_coyote_in(root, root, args)
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
/// missing host config without consulting the stub.
fn sandbox_bails_on_missing_config(label: &str, root: &Path, log: &Path) {
    let output = run_coyote(root, &["--sandbox", "probe"]);
    assert!(!output.status.success(), "{label}: expected a refusal");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("No Coyote config found"),
        "{label}: unexpected stderr:\n{stderr}"
    );
    assert!(
        !log.exists(),
        "{label}: the stub sbx was invoked:\n{}",
        stub_calls(log).join("\n")
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
fn usage_probe_sandbox_refuses_a_malformed_kit_before_asking_sbx_for_sandboxes() {
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
        calls
            .iter()
            .all(|call| call != "ls" && !call.starts_with("create ") && !call.starts_with("run ")),
        "discovery must abort before any sandbox is listed or created: {calls:?}"
    );
    // The base kit was still extracted: discovery runs after the kit cache
    // is settled.
    assert_kit_matches_assets("bad kit run", &root);
}

#[test]
fn usage_probe_sandbox_passes_discovered_kits_to_create_in_root_order_and_ignores_legacy_files() {
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

    // Leftovers from the previous kit grammar at every place it used to
    // look: ignored, never discovered, never warned about.
    let legacy_yaml = "name: legacy\nschemaVersion: \"2\"\nkind: mixin\n";
    for path in [
        config.join(LEGACY_FILE),
        config.join("functions").join(LEGACY_FILE),
        config.join("functions").join("x").join(LEGACY_FILE),
        config.join("agents").join("researcher").join(LEGACY_FILE),
        config
            .join("agents")
            .join("researcher")
            .join(format!("docs.{LEGACY_FILE}")),
        config.join("hooks").join(format!("h.{LEGACY_FILE}")),
        workspace.join(".coyote").join(LEGACY_FILE),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, legacy_yaml).unwrap();
    }
    // A stray file beside the kit dirs is ignored too.
    fs::write(config.join("kits").join("README.md"), "about\n").unwrap();

    let output = run_coyote_in(&root, &cwd, &["--sandbox", "probe", "--fresh"]);

    let stderr = stderr_of(&output);
    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "expected the stubbed launch to succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let calls = stub_calls(&log);
    assert_eq!(
        create_kit_args(&calls),
        vec![
            kit_dir(&root).display().to_string(),
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
        "base kit first, then every discovered kit in root order: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("run --name probe ")),
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

    // No surface mentions the legacy files: not stderr, not the log file.
    let log_text = fs::read_to_string(root.join("cache").join("coyote.log")).unwrap_or_default();
    for (surface, text) in [("stderr", &stderr), ("stdout", &stdout), ("log", &log_text)] {
        assert!(
            !text.contains(LEGACY_FILE) && !text.contains("legacy"),
            "{surface} mentions the ignored legacy files:\n{text}"
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
