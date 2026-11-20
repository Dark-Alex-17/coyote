//! Real-process checks of the pty harness itself (`tests/common/pty.rs`):
//! the child a spawn drives inherits no descriptor the test runner holds,
//! and both halves of the pair are close-on-exec from the moment they are
//! opened. The children are `sh` and `bash`; no coyote binary is involved,
//! so the file pins no config or cache dir. The cases run one at a time:
//! each spawn forks a session leader and takes a controlling tty, and a
//! fork landing between another test's `openpty` and its `exec` has
//! failed the spawn with a garbled errno.
//! Next to them, two source scans: every non-exempt `tui_*.rs` suite
//! imports the shared pty harness rather than a copy of it, and none of
//! them declares an item that shadows one the harness exports.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

use common::pty::Pty;
use common::source_scan::read;
use serial_test::serial;

const PTY_DEADLINE: Duration = Duration::from_secs(20);

/// Spelled in halves: `binary_harness_pins.rs` counts every file whose
/// code names the harness module as one of its importers.
const HARNESS_IMPORT: &str = concat!("common::tui_", "harness");
/// `tui_wiki_docs.rs` only reads sources and docs. `tui_cli_cold_start.rs`
/// runs on every CI leg, Windows included, and the harness is unix-only.
const HARNESS_EXEMPT: [&str; 2] = ["tui_cli_cold_start.rs", "tui_wiki_docs.rs"];
/// Kinds of item a suite's glob import of the harness can bring in, and
/// so the kinds a same-named local item would silently shadow.
const ITEM_KINDS: [&str; 5] = ["fn", "const", "static", "struct", "enum"];

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir(scenario: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-pty-hygiene-{scenario}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Nothing the test runner holds leaks into the child, neither side of
/// the pty nor an ordinary file opened without close-on-exec: it sees
/// only its three standard streams and, on the lowest free numbers, `ls`'s
/// own handles: one for GNU `ls`, two for BSD `ls`. On a tty `ls` prints
/// its entries in columns, so the list is read word by word. Two
/// placeholders keep the scratch file off fds 3 and 4, where `ls`'s own
/// handles would be mistaken for it.
#[test]
#[serial]
fn the_child_inherits_no_pty_descriptor() {
    use std::os::fd::AsRawFd;

    fn listed_fds(text: &str) -> std::collections::BTreeSet<u32> {
        text.split_whitespace()
            .take_while(|word| *word != "fd-list-done")
            .map(|word| word.parse().unwrap())
            .collect()
    }
    let dir = scratch_dir("fd-leak");
    let _guard = TempDirGuard(dir.clone());
    let _placeholders = [fs::File::open(&dir).unwrap(), fs::File::open(&dir).unwrap()];
    let scratch = fs::File::create(dir.join("scratch")).unwrap();
    // SAFETY: fcntl with F_SETFD takes no pointers; the result is checked.
    assert_ne!(
        unsafe { libc::fcntl(scratch.as_raw_fd(), libc::F_SETFD, 0) },
        -1
    );
    let scratch_fd = u32::try_from(scratch.as_raw_fd()).unwrap();
    assert!(
        scratch_fd > 4,
        "the placeholders did not keep the scratch file off fds 3 and 4"
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("ls /dev/fd; echo fd-list-done");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty
        .wait_for("fd-list-done", Duration::from_secs(10))
        .unwrap();
    let on_pty = listed_fds(&transcript);
    assert!(
        !on_pty.contains(&scratch_fd),
        "the parent's scratch file (fd {scratch_fd}) reached the pty child:\n{transcript}"
    );
    assert!(
        on_pty == (0..=3).collect() || on_pty == (0..=4).collect(),
        "the pty child sees more than its standard streams and ls's own fds:\n{transcript}"
    );
}

fn has_cloexec(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: fcntl with F_GETFD takes no pointers; -1 reads as "not set".
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags != -1 && flags & libc::FD_CLOEXEC != 0
}

/// Both halves of the pair carry close-on-exec from the moment they are
/// opened, and the master keeps it across the spawn, so a child that is
/// not the one being driven cannot inherit either end.
#[test]
#[serial]
fn the_pty_pair_is_close_on_exec_on_both_sides() {
    use std::os::fd::AsRawFd;

    let (master, slave) = common::pty::open_pair(40, 100).unwrap();
    assert!(has_cloexec(master.as_raw_fd()), "master lacks FD_CLOEXEC");
    assert!(has_cloexec(slave.as_raw_fd()), "slave lacks FD_CLOEXEC");
    drop((master, slave));

    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg("echo spawned-ok");
    let mut pty = Pty::spawn(cmd).unwrap();
    assert!(
        has_cloexec(pty.master_fd()),
        "the master lost FD_CLOEXEC across the spawn"
    );
    pty.wait_for("spawned-ok", PTY_DEADLINE).unwrap();
}

/// The `tui_*.rs` suites directly under `tests/`; the harness itself,
/// under `tests/common/`, is the one place that spawns for them.
fn tui_suites(tests_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(tests_dir)
        .expect("tests dir is readable")
        .map(|entry| entry.expect("tests dir entry").path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "rs")
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("tui_"))
        })
        .collect();
    out.sort();
    out
}

/// The source without its comment lines, so a comment that mentions the
/// harness import is not mistaken for one, and an item head quoted in a
/// doc comment is not scanned as a declared item.
fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The name an item head declares when `line`, at any indent, opens a
/// `fn`, `const`, `static`, `struct` or `enum`, whatever its visibility.
fn item_name(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace().peekable();
    if words.peek().is_some_and(|word| word.starts_with("pub")) {
        words.next();
    }
    while words
        .peek()
        .is_some_and(|word| ["async", "unsafe", "extern"].contains(word))
    {
        words.next();
    }
    let mut kind = words.next()?;
    if kind == "const" && words.peek() == Some(&"fn") {
        kind = words.next()?;
    }
    if !ITEM_KINDS.contains(&kind) {
        return None;
    }
    let mut rest = words.next()?;
    if kind == "static" && rest == "mut" {
        rest = words.next()?;
    }
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// Every non-exempt `tui_*.rs` suite imports the shared harness, so a
/// suite that spawns cannot carry a private copy of the spawn that pins
/// the config and cache dirs. The count of importers is checked against
/// the suites found, so a scan that matched nothing cannot pass. It runs
/// serially as well: the files it opens would shift the descriptor numbers
/// the fd-leak case relies on.
#[test]
#[serial]
fn every_tui_suite_imports_the_shared_harness() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut suites = Vec::new();
    let mut harness_users = Vec::new();
    let mut unharnessed = Vec::new();
    for path in tui_suites(&tests_dir) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let code = code_only(&read(&path));
        if code.contains(HARNESS_IMPORT) {
            harness_users.push(name.clone());
        } else if !HARNESS_EXEMPT.contains(&name.as_str()) {
            unharnessed.push(name.clone());
        }
        suites.push(name);
    }
    assert!(
        unharnessed.is_empty(),
        "these suites do not import the shared harness: {unharnessed:?}"
    );
    let exempt: Vec<&String> = suites
        .iter()
        .filter(|name| HARNESS_EXEMPT.contains(&name.as_str()))
        .collect();
    assert_eq!(
        exempt.len(),
        HARNESS_EXEMPT.len(),
        "an exempt suite is gone from tests/: found {exempt:?} of {HARNESS_EXEMPT:?}"
    );
    assert!(
        !suites.is_empty() && harness_users.len() >= suites.len() - exempt.len(),
        "{} harness users among {} suites, {} of them exempt\nusers: {harness_users:?}\nsuites: {suites:?}",
        harness_users.len(),
        suites.len(),
        exempt.len()
    );
}

/// A suite's `use common::tui_harness::*` loses silently to any local item
/// of the same name, so a local variant that behaves differently would
/// swap meaning without a compile error the day the copy is deleted. Only
/// the harness's top-level `pub` items count; suites' items count at any
/// indent, since the suites group their cases into inline modules.
#[test]
#[serial]
fn no_tui_suite_shadows_a_harness_item() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let harness_file = tests_dir.join("common").join(concat!("tui_", "harness.rs"));
    let harness = code_only(&read(harness_file));
    let exported: Vec<&str> = harness
        .lines()
        .filter(|line| line.starts_with("pub "))
        .filter_map(item_name)
        .collect();
    assert!(
        exported.len() >= 50,
        "the harness scan found only {} pub items: {exported:?}",
        exported.len()
    );
    let mut shadows = Vec::new();
    for path in tui_suites(&tests_dir) {
        let code = code_only(&read(&path));
        if !code.contains(HARNESS_IMPORT) {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for item in code.lines().filter_map(item_name) {
            if exported.contains(&item) {
                shadows.push(format!("{name}: {item}"));
            }
        }
    }
    assert!(
        shadows.is_empty(),
        "these suite items shadow a harness item of the same name; rename them:\n{}",
        shadows.join("\n")
    );
}
