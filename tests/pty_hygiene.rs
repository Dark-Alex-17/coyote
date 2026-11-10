//! Real-process checks of the pty harness itself (`tests/common/pty.rs`):
//! the child a spawn drives inherits no descriptor the test runner holds,
//! and both halves of the pair are close-on-exec from the moment they are
//! opened. The children are `sh` and `bash`; no coyote binary is involved,
//! so the file pins no config or cache dir. The cases run one at a time:
//! each spawn forks a session leader and takes a controlling tty, and a
//! fork landing between another test's `openpty` and its `exec` has
//! failed the spawn with a garbled errno.
#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

use common::pty::Pty;
use serial_test::serial;

const PTY_DEADLINE: Duration = Duration::from_secs(20);

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
/// only its three standard streams and, on the lowest free number, `ls`'s
/// own handle on the directory it lists. On a tty `ls` prints its entries
/// in columns, so the list is read word by word. A placeholder keeps the
/// scratch file off fd 3, where `ls`'s own handle would be mistaken for it.
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
    let _placeholder = fs::File::open(&dir).unwrap();
    let scratch = fs::File::create(dir.join("scratch")).unwrap();
    // SAFETY: fcntl with F_SETFD takes no pointers; the result is checked.
    assert_ne!(
        unsafe { libc::fcntl(scratch.as_raw_fd(), libc::F_SETFD, 0) },
        -1
    );
    let scratch_fd = u32::try_from(scratch.as_raw_fd()).unwrap();
    assert_ne!(
        scratch_fd, 3,
        "the placeholder did not keep the scratch file off fd 3"
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
    assert_eq!(
        on_pty,
        (0..=3).collect(),
        "the pty child sees more than its standard streams and ls's own directory fd:\n{transcript}"
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
