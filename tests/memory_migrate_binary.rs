//! Cold-start runs of `--memory-migrate` through the real binary against a
//! fresh temp config dir and cache dir. The global store lives under the
//! config dir and the workspace store under `ws/.coyote/memory`; backups land
//! under `<cache>/memory-backups`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Mirrors `WORKSPACE_MEMORY_ROOT_TEST_ENV` in `src/config/memory.rs`;
/// `the_test_memory_root_env_name_matches_the_source_const` keeps the two
/// equal.
const TEST_WORKSPACE_MEMORY_ROOT_ENV: &str = "COYOTE_TEST_WORKSPACE_MEMORY_ROOT";

struct Fixture {
    dir: PathBuf,
    workspace: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn global_store(&self) -> PathBuf {
        self.dir.join("memory")
    }

    fn workspace_store(&self) -> PathBuf {
        self.workspace.join(".coyote").join("memory")
    }

    fn cache(&self) -> PathBuf {
        self.dir.join("cache")
    }

    fn backups(&self) -> PathBuf {
        self.cache().join("memory-backups")
    }
}

fn fixture(label: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "coyote-memory-migrate-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    let fx = Fixture {
        workspace: dir.join("ws"),
        dir,
    };
    fs::create_dir_all(fx.dir.join("home")).unwrap();
    fs::write(fx.dir.join("config.yaml"), "model: [not, a, string]\n").unwrap();
    for (store, slug, desc, extra_line) in [
        (fx.global_store(), "g-note", "global note", ""),
        (fx.workspace_store(), "w-note", "workspace note", GHOST_LINE),
    ] {
        fs::create_dir_all(&store).unwrap();
        fs::write(
            store.join("MEMORY.md"),
            format!(
                "## Always-on facts\n- **Fact.** kept\n\n## Drill files\n- [[{slug}]]: {desc}\n\
                 {extra_line}"
            ),
        )
        .unwrap();
        fs::write(
            store.join(format!("{slug}.md")),
            format!("---\nname: {slug}\nowner: alice\ntype: project\n---\n\nbody\n"),
        )
        .unwrap();
    }
    fs::write(fx.global_store().join(CATALOGUE_CACHE), "cache\n").unwrap();
    fx
}

const GHOST_LINE: &str = "- [[ghost]]: missing\n";
const CATALOGUE_CACHE: &str = ".catalogue.yaml";

fn command(fx: &Fixture, args: &[&str]) -> Command {
    let home = fx.dir.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_coyote"));
    command
        .args(args)
        .current_dir(&fx.workspace)
        .env("COYOTE_CONFIG_DIR", &fx.dir)
        .env("COYOTE_CACHE_DIR", fx.cache())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM");
    command
}

fn run(fx: &Fixture, args: &[&str]) -> Output {
    command(fx, args).stdin(Stdio::null()).output().unwrap()
}

/// Every file under `dir` keyed by its path relative to `dir`; empty when
/// `dir` does not exist.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn assert_success(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(
        output.status.success(),
        "status {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// The directory text the report prints on its `<scope>: <dir>` header line.
fn header_dir<'a>(stdout: &'a str, scope: &str) -> &'a str {
    let prefix = format!("{scope}: ");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {scope} header in\n{stdout}"))
}

#[test]
fn report_only_leaves_the_stores_and_the_backups_untouched() {
    let fx = fixture("report");
    let global_before = snapshot(&fx.global_store());
    let workspace_before = snapshot(&fx.workspace_store());

    let stdout = assert_success(&run(&fx, &["--memory-migrate", "all"]));

    assert!(stdout.contains("rerun with --apply to migrate"), "{stdout}");
    assert_eq!(snapshot(&fx.global_store()), global_before);
    assert_eq!(snapshot(&fx.workspace_store()), workspace_before);
    assert!(!fx.backups().exists());
}

#[test]
fn an_invalid_round_chains_setting_fails_the_run_untouched() {
    let fx = fixture("bad_round_chains");
    fs::write(fx.dir.join("config.yaml"), "memory_round_chains: bogus\n").unwrap();
    let global_before = snapshot(&fx.global_store());
    let workspace_before = snapshot(&fx.workspace_store());

    for args in [
        &["--memory-migrate", "all"][..],
        &["--memory-migrate", "all", "--apply"][..],
    ] {
        let output = run(&fx, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}\nstderr:\n{stderr}");
        assert!(
            stderr.contains(
                "memory_round_chains could not be read; fix the config and rerun --memory-migrate"
            ),
            "{args:?}\nstderr:\n{stderr}"
        );
    }
    assert_eq!(snapshot(&fx.global_store()), global_before);
    assert_eq!(snapshot(&fx.workspace_store()), workspace_before);
    assert!(!fx.backups().exists());
}

#[test]
fn a_keep_round_chains_setting_leaves_older_rounds_unmarked() {
    let fx = fixture("keep_round_chains");
    fs::write(
        fx.dir.join("config.yaml"),
        "memory_round_chains: keep\nmodel: [not, a, string]\n",
    )
    .unwrap();
    let store = fx.workspace_store();
    for round in ["plan-r1", "plan-r2"] {
        fs::write(
            store.join(format!("{round}.md")),
            format!("---\nname: {round}\ndescription: {round}\ntype: run-state\n---\n\nbody\n"),
        )
        .unwrap();
    }

    let stdout = assert_success(&run(&fx, &["--memory-migrate", "all", "--apply"]));

    let older = fs::read_to_string(store.join("plan-r1.md")).unwrap();
    assert!(
        !older.contains("superseded_by"),
        "{older}\nstdout:\n{stdout}"
    );
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn piped_stdin_is_not_read_as_a_prompt() {
    let fx = fixture("stdin");
    let mut child = KillOnDrop(
        command(&fx, &["--memory-migrate", "all"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // Held open until the end: a child that reads stdin to EOF never exits.
    let mut stdin = child.0.stdin.take().unwrap();
    stdin.write_all(b"summarise this\n").unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "still running with stdin open after 10s"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stdout = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();

    assert!(status.success(), "status {status:?}\nstdout:\n{stdout}");
    assert!(stdout.contains("rerun with --apply to migrate"), "{stdout}");
}

#[test]
fn apply_backs_up_both_stores_and_rewrites_them() {
    let fx = fixture("apply");
    let global_before = snapshot(&fx.global_store());
    let workspace_before = snapshot(&fx.workspace_store());

    let date_before = chrono::Local::now().format("%Y-%m-%d").to_string();
    let stdout = assert_success(&run(&fx, &["--memory-migrate", "all", "--apply"]));
    let date_after = chrono::Local::now().format("%Y-%m-%d").to_string();

    let mut backups: Vec<String> = fs::read_dir(fx.backups())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    backups.sort();
    assert_eq!(backups.len(), 2, "{backups:?}");
    assert!(backups[0].starts_with("global-"), "{backups:?}");
    assert!(backups[1].starts_with("workspace-"), "{backups:?}");
    let mut global_backed_up = global_before.clone();
    assert!(
        global_backed_up
            .remove(Path::new(CATALOGUE_CACHE))
            .is_some()
    );
    assert_eq!(
        snapshot(&fx.backups().join(&backups[0])),
        global_backed_up,
        "{stdout}"
    );
    assert_eq!(snapshot(&fx.backups().join(&backups[1])), workspace_before);
    let tail = format!(
        "\nbackup: {}\nbackup: {}\n\
         next: review the remaining sections by hand; migrate leaves them as they are\n",
        fx.backups().join(&backups[0]).display(),
        fx.backups().join(&backups[1]).display()
    );
    assert!(stdout.ends_with(&tail), "{tail:?} does not end\n{stdout}");
    let global_backup = stdout
        .lines()
        .find_map(|line| line.strip_prefix("backup: "))
        .unwrap();
    assert!(!Path::new(global_backup).join(CATALOGUE_CACHE).exists());
    assert!(fx.global_store().join(CATALOGUE_CACHE).exists());

    for (store, label, slug, desc, index) in [
        (
            fx.global_store(),
            "global",
            "g-note",
            "global note",
            "## Always-on facts\n- **Fact.** kept\n".to_string(),
        ),
        (
            fx.workspace_store(),
            "workspace",
            "w-note",
            "workspace note",
            format!("## Always-on facts\n- **Fact.** kept\n\n## Drill files\n{GHOST_LINE}"),
        ),
    ] {
        assert_eq!(fs::read_to_string(store.join("MEMORY.md")).unwrap(), index);
        let printed = Path::new(header_dir(&stdout, label));
        for written in [
            printed.join("MEMORY.md"),
            printed.join(format!("{slug}.md")),
        ] {
            let line = format!("    wrote {}\n", written.display());
            assert!(stdout.contains(&line), "{line:?} missing from\n{stdout}");
        }
        let drill = fs::read_to_string(store.join(format!("{slug}.md"))).unwrap();
        let expected = |today: &str| {
            format!(
                "---\nname: {slug}\nowner: alice\ntype: project\ndescription: {desc}\n\
                 updated: {today}\n---\n\nbody\n"
            )
        };
        assert!(
            drill == expected(&date_before) || drill == expected(&date_after),
            "{drill}"
        );
    }
}

#[cfg(unix)]
struct RestoreMode(PathBuf);

#[cfg(unix)]
impl Drop for RestoreMode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

#[cfg(unix)]
#[test]
fn a_workspace_that_cannot_be_written_stops_after_global_and_names_both_backups() {
    use std::os::unix::fs::PermissionsExt;

    let fx = fixture("readonly");
    let store = fx.workspace_store();
    fs::set_permissions(&store, fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = RestoreMode(store.clone());
    if fs::write(store.join("probe"), "").is_ok() {
        eprintln!("skipped: this user can write a 0500 dir, so the failure cannot be staged");
        return;
    }
    let workspace_before = snapshot(&store);

    let output = run(&fx, &["--memory-migrate", "all", "--apply"]);

    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(snapshot(&store), workspace_before, "{stdout}");
    let global_index = Path::new(header_dir(&stdout, "global")).join("MEMORY.md");
    let line = format!("    wrote {}\n", global_index.display());
    assert!(stdout.contains(&line), "{line:?} missing from\n{stdout}");
    let backup = |label: &str| {
        let name = fs::read_dir(fx.backups())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(&format!("{label}-")))
            .unwrap_or_else(|| panic!("no {label} backup\n{stdout}"));
        fx.backups().join(name)
    };
    let (global_backup, workspace_backup) = (backup("global"), backup("workspace"));
    let lines: Vec<&str> = stdout.lines().collect();
    let [stopped, tail @ ..] = &lines[lines.len().saturating_sub(4)..] else {
        panic!("short report\n{stdout}");
    };
    assert!(stopped.starts_with("stopped: workspace: "), "{stdout}");
    assert_eq!(
        tail,
        [
            format!("backup: {}", global_backup.display()),
            format!("backup: {}", workspace_backup.display()),
            "next: fix the error and rerun --memory-migrate; each backup above holds its \
             store as it was before this run"
                .to_string(),
        ],
        "{stdout}"
    );
    assert!(
        stderr.contains(&format!(
            "workspace migration stopped partway; its backup is at {}",
            workspace_backup.display()
        )),
        "{stderr}"
    );
}

/// The unit-test workspace memory pin is a test-build seam: a real
/// `--memory-migrate --apply` that carries it still migrates the store above
/// its cwd and leaves the pinned workspace byte-for-byte untouched.
#[test]
fn usage_probe_apply_migrates_the_cwd_workspace_and_ignores_the_test_root_pin() {
    let fx = fixture("pin");
    let decoy = fx.dir.join("decoy");
    let decoy_store = decoy.join(".coyote").join("memory");
    fs::create_dir_all(&decoy_store).unwrap();
    fs::write(
        decoy_store.join("MEMORY.md"),
        "## Drill files\n- [[d-note]]: decoy note\n",
    )
    .unwrap();
    fs::write(
        decoy_store.join("d-note.md"),
        "---\nname: d-note\nowner: alice\ntype: project\n---\n\nbody\n",
    )
    .unwrap();
    let decoy_before = snapshot(&decoy);

    let output = command(&fx, &["--memory-migrate", "all", "--apply"])
        .env(TEST_WORKSPACE_MEMORY_ROOT_ENV, &decoy)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = assert_success(&output);

    let printed = Path::new(header_dir(&stdout, "workspace"));
    assert!(
        fs::canonicalize(printed).unwrap() == fs::canonicalize(fx.workspace_store()).unwrap(),
        "workspace header {printed:?} is not the cwd store\n{stdout}"
    );
    assert!(!stdout.contains("d-note"), "{stdout}");
    assert_eq!(
        snapshot(&decoy),
        decoy_before,
        "the pinned root was rewritten"
    );
    let index = fs::read_to_string(fx.workspace_store().join("MEMORY.md")).unwrap();
    assert!(
        !index.contains("[[w-note]]"),
        "cwd store not migrated:\n{index}"
    );
    let drill = fs::read_to_string(fx.workspace_store().join("w-note.md")).unwrap();
    assert!(
        drill.contains("description: workspace note"),
        "cwd drill not migrated:\n{drill}"
    );
}

/// `TEST_WORKSPACE_MEMORY_ROOT_ENV` is a copy of the source const, so a
/// rename there would leave the probe above setting a variable no build
/// reads; this keeps the two equal.
#[test]
fn the_test_memory_root_env_name_matches_the_source_const() {
    let source_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config/memory.rs");
    let source = fs::read_to_string(&source_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", source_path.display()));
    let needle =
        format!("WORKSPACE_MEMORY_ROOT_TEST_ENV: &str = \"{TEST_WORKSPACE_MEMORY_ROOT_ENV}\"");
    assert!(
        source.contains(&needle),
        "{} no longer declares {needle}",
        source_path.display()
    );
}
