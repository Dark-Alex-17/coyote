//! Cold-start runs of `--memory-migrate` through the real binary against a
//! fresh temp config dir and cache dir. The global store lives under the
//! config dir and the workspace store under `ws/.coyote/memory`; backups land
//! under `<cache>/memory-backups`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    fs::write(fx.dir.join("config.yaml"), "model: [unclosed\n").unwrap();
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
    fx
}

const GHOST_LINE: &str = "- [[ghost]]: missing\n";

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
fn piped_stdin_is_not_read_as_a_prompt() {
    let fx = fixture("stdin");
    let mut child = command(&fx, &["--memory-migrate", "all"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"summarise this\n").unwrap();
    drop(stdin);

    let stdout = assert_success(&child.wait_with_output().unwrap());

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
    assert_eq!(
        snapshot(&fx.backups().join(&backups[0])),
        global_before,
        "{stdout}"
    );
    assert_eq!(snapshot(&fx.backups().join(&backups[1])), workspace_before);
    for name in &backups {
        let line = format!("  backup: {}\n", fx.backups().join(name).display());
        assert!(stdout.contains(&line), "{line:?} missing from\n{stdout}");
    }
    assert!(
        stdout.ends_with(
            "next: review the remaining sections by hand; migrate leaves them as they are\n"
        ),
        "{stdout}"
    );

    for (store, slug, desc, index) in [
        (
            fx.global_store(),
            "g-note",
            "global note",
            "## Always-on facts\n- **Fact.** kept\n".to_string(),
        ),
        (
            fx.workspace_store(),
            "w-note",
            "workspace note",
            format!("## Always-on facts\n- **Fact.** kept\n\n## Drill files\n{GHOST_LINE}"),
        ),
    ] {
        assert_eq!(fs::read_to_string(store.join("MEMORY.md")).unwrap(), index);
        let drill = fs::read_to_string(store.join(format!("{slug}.md"))).unwrap();
        let expected = |today: &str| {
            format!(
                "---\nname: {slug}\nowner: alice\ntype: project\ndescription: {desc}\n\
                 updated: {today}\n---\n\nbody\n"
            )
        };
        assert_eq!(
            drill,
            if drill == expected(&date_after) {
                expected(&date_after)
            } else {
                expected(&date_before)
            }
        );
    }
}
