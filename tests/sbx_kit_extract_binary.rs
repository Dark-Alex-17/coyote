//! Cold-start, real-binary contract for the embedded sandbox kit: the
//! `--info` readout names the kits cache root, and `--sandbox` extracts the
//! base kit into `<cache>/sbx-kits/coyote/` (plus the sibling hash file)
//! BEFORE it checks for a host config, so a config-less run leaves the
//! extracted layout behind and bails without ever calling `sbx`.
//!
//! `sbx` is satisfied by a stub on PATH that records every invocation; the
//! config-less path must never reach it. Every run uses a fresh fake HOME,
//! config dir and cache dir so host state cannot leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

const KIT_YAML: &str = "coyote.yaml";
const KIT_DOCKERFILE: &str = "coyote.dockerfile";
const KITS_DIR: &str = "sbx-kits";
const HANDLE: &str = "coyote";

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
    let dir = env::temp_dir().join(format!("coyote-sbx-kit-{label}-{}-{unique}", process::id()));
    for sub in ["home", "config", "cache", "bin"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    dir
}

fn repo_asset(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/sbx-kit")
        .join(HANDLE)
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Writes a dry-run config so `--info` has something to report; the
/// `--sandbox` tests deliberately leave the config dir empty.
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

/// An `sbx` that satisfies the PATH lookup and records any call it receives.
fn install_stub_sbx(root: &Path) -> PathBuf {
    let marker = root.join("stub-sbx-invoked");
    let stub = root.join("bin/sbx");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 97\n",
            marker.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    marker
}

fn run_coyote(root: &Path, args: &[&str]) -> Output {
    let home = root.join("home");
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .current_dir(root)
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

/// Runs the config-less `--sandbox` path and asserts it bailed on the
/// missing host config without consulting the stub.
fn sandbox_bails_on_missing_config(label: &str, root: &Path, marker: &Path) {
    let output = run_coyote(root, &["--sandbox", "probe"]);
    assert!(
        !output.status.success(),
        "{label}: expected a refusal, got {:?}\nstdout: {}",
        output.status,
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("No Coyote config found"),
        "{label}: unexpected stderr:\n{stderr}"
    );
    assert!(
        !marker.exists(),
        "{label}: the stub sbx was invoked:\n{}",
        fs::read_to_string(marker).unwrap_or_default()
    );
}

/// Every regular file under the kits cache root, relative to it.
fn cache_listing(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(&kits_root(root), &kits_root(root), &mut out);
    out.sort();
    out
}

fn assert_extracted_matches_assets(label: &str, root: &Path) {
    assert_eq!(
        fs::read(kit_dir(root).join(KIT_YAML)).unwrap(),
        repo_asset(KIT_YAML),
        "{label}: extracted {KIT_YAML} differs from the shipped asset"
    );
    assert_eq!(
        fs::read(kit_dir(root).join(KIT_DOCKERFILE)).unwrap(),
        repo_asset(KIT_DOCKERFILE),
        "{label}: extracted {KIT_DOCKERFILE} differs from the shipped asset"
    );
}

fn read_hash(root: &Path) -> String {
    fs::read_to_string(hash_file(root)).unwrap()
}

#[test]
fn info_reports_the_kits_cache_root_under_the_new_row_name() {
    let root = fresh_root("info");
    let _cleanup = TempDirGuard(root.clone());
    write_dry_run_config(&root);

    let output = run_coyote(&root, &["--info"]);
    assert!(
        output.status.success(),
        "--info failed {:?}\nstderr: {}",
        output.status,
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    let row_key = |line: &str| line.split_whitespace().next().unwrap_or("").to_string();
    let rows: Vec<&str> = stdout
        .lines()
        .filter(|line| row_key(line).starts_with("sbx_kit"))
        .collect();
    assert_eq!(rows.len(), 1, "exactly one sbx kit row expected:\n{stdout}");
    assert_eq!(row_key(rows[0]), "sbx_kits_dir", "{stdout}");
    let value = rows[0]["sbx_kits_dir".len()..].trim();
    assert_eq!(
        Path::new(value).file_name().and_then(|n| n.to_str()),
        Some(KITS_DIR),
        "{}",
        rows[0]
    );
    assert!(
        Path::new(value).starts_with(root.join("cache")),
        "row must point inside the configured cache dir: {}",
        rows[0]
    );
    // Reporting the path must not create it.
    assert!(!kits_root(&root).exists(), "{stdout}");
}

#[test]
fn sandbox_extracts_the_base_kit_before_refusing_a_config_less_host() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("extract");
    let _cleanup = TempDirGuard(root.clone());
    let marker = install_stub_sbx(&root);

    sandbox_bails_on_missing_config("first run", &root, &marker);

    assert_eq!(
        cache_listing(&root),
        vec![
            format!("{HANDLE}.sha256"),
            format!("{HANDLE}/{KIT_DOCKERFILE}"),
            format!("{HANDLE}/{KIT_YAML}"),
        ],
        "extraction must leave exactly the kit dir plus its sibling hash file"
    );
    assert_extracted_matches_assets("first run", &root);
    let hash = read_hash(&root);
    assert_eq!(hash.len(), 64, "{hash:?}");
    assert!(
        hash.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{hash:?}"
    );

    // A second cold cache derives the same hash: it is a function of the
    // embedded bytes alone, never of the machine or the moment.
    let twin = fresh_root("extract-twin");
    let _cleanup_twin = TempDirGuard(twin.clone());
    let twin_marker = install_stub_sbx(&twin);
    sandbox_bails_on_missing_config("twin run", &twin, &twin_marker);
    assert_eq!(read_hash(&twin), hash);
}

#[test]
fn sandbox_reextracts_only_when_the_hash_or_the_kit_dir_is_gone() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("tamper");
    let _cleanup = TempDirGuard(root.clone());
    let marker = install_stub_sbx(&root);
    sandbox_bails_on_missing_config("seed", &root, &marker);
    let dockerfile = kit_dir(&root).join(KIT_DOCKERFILE);
    let good_hash = read_hash(&root);

    // A valid local edit survives while the recorded hash still matches.
    fs::write(&dockerfile, "FROM scratch\n").unwrap();
    sandbox_bails_on_missing_config("hash match", &root, &marker);
    assert_eq!(fs::read_to_string(&dockerfile).unwrap(), "FROM scratch\n");
    assert_eq!(read_hash(&root), good_hash);

    // Losing the kit dir out-of-band re-extracts even though the hash
    // file is intact.
    fs::remove_dir_all(kit_dir(&root)).unwrap();
    sandbox_bails_on_missing_config("dir missing", &root, &marker);
    assert_extracted_matches_assets("dir missing", &root);

    // A stale hash rewrites the kit dir wholesale (edits and junk go) but
    // leaves every sibling under the kits root untouched.
    fs::write(&dockerfile, "FROM scratch\n").unwrap();
    fs::write(kit_dir(&root).join("junk"), b"stale").unwrap();
    fs::write(hash_file(&root), "stale").unwrap();
    for sibling in ["other/x", "created/m"] {
        let path = kits_root(&root).join(sibling);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, sibling.as_bytes()).unwrap();
    }
    sandbox_bails_on_missing_config("stale hash", &root, &marker);
    assert_extracted_matches_assets("stale hash", &root);
    assert_eq!(read_hash(&root), good_hash);
    assert!(!kit_dir(&root).join("junk").exists());
    for sibling in ["other/x", "created/m"] {
        assert_eq!(
            fs::read(kits_root(&root).join(sibling)).unwrap(),
            sibling.as_bytes(),
            "{sibling} must survive a base-kit rewrite"
        );
    }
}

#[test]
fn sandbox_kit_override_pointing_nowhere_is_refused_before_any_extraction() {
    if cfg!(windows) {
        eprintln!("skipping: the stub sbx is a POSIX shell script");
        return;
    }
    let root = fresh_root("override");
    let _cleanup = TempDirGuard(root.clone());
    let marker = install_stub_sbx(&root);
    let missing = root.join("no-such-kit");

    let output = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(["--sandbox", "probe"])
        .current_dir(&root)
        .env("COYOTE_CONFIG_DIR", root.join("config"))
        .env("COYOTE_CACHE_DIR", root.join("cache"))
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.join("bin").display(),
                env::var("PATH").unwrap_or_default()
            ),
        )
        .env("COYOTE_SANDBOX_KIT", &missing)
        .env("NO_COLOR", "1")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("$COYOTE_SANDBOX_KIT is set but path does not exist"),
        "{stderr}"
    );
    assert!(stderr.contains(&missing.display().to_string()), "{stderr}");
    assert!(
        !kits_root(&root).exists(),
        "override must not touch the cache"
    );
    assert!(!marker.exists());
}
