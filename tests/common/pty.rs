use std::process::Command;

/// Detaches the child from the test runner's controlling terminal, so its
/// `/dev/tty` probe fails and terminal-size lookups fall through to
/// whatever `tput` is on PATH.
#[cfg(unix)]
pub fn detached(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

#[cfg(windows)]
pub fn detached(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)
}

/// A child running with a pseudo-terminal as its controlling tty and as
/// all three standard streams; the test holds the master side.
#[cfg(unix)]
pub struct Pty {
    master: std::fs::File,
    child: std::process::Child,
    raw: Vec<u8>,
}

#[cfg(unix)]
impl Pty {
    /// Spawns on a 100x40 terminal, the size the fixture `tput` reports.
    pub fn spawn(mut cmd: Command) -> std::io::Result<Pty> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;

        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: both out-pointers are valid ints, `size` is a valid
        // winsize; name and termios may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openpty just created both descriptors and nothing else owns them.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let master_fd = master.as_raw_fd();
        let slave_fd = slave.as_raw_fd();
        for fd in [master_fd, slave_fd] {
            // SAFETY: fcntl with F_SETFD takes no pointers; the result is checked.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        // SAFETY: the hook calls only async-signal-safe libc functions on
        // descriptors that stay open in the parent until after spawn returns.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // The request constant's integer type differs between libc targets.
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // dup2 clears close-on-exec on 0, 1 and 2; the originals
                // still carry it, and closing them here is belt and braces.
                for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    if libc::dup2(slave_fd, fd) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                libc::close(master_fd);
                libc::close(slave_fd);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(slave);
        Ok(Pty {
            master: std::fs::File::from(master),
            child,
            raw: Vec::new(),
        })
    }

    /// Reads the master until the stripped transcript contains `needle` and
    /// returns the whole stripped transcript so far.
    pub fn wait_for(
        &mut self,
        needle: &str,
        timeout: std::time::Duration,
    ) -> std::io::Result<String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let text = strip_ansi(&String::from_utf8_lossy(&self.raw));
            if text.contains(needle) {
                return Ok(text);
            }
            self.pump(deadline, timeout, needle, &text)?;
        }
    }

    /// Reads the master until the raw (unstripped) bytes contain `needle`
    /// at or after offset `from`, and returns the index where it starts.
    /// Lets a test assert the order of terminal control sequences, which
    /// `wait_for` strips away.
    pub fn wait_for_raw(
        &mut self,
        needle: &[u8],
        from: usize,
        timeout: std::time::Duration,
    ) -> std::io::Result<usize> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(at) = find_bytes(&self.raw[from.min(self.raw.len())..], needle) {
                return Ok(from + at);
            }
            let text = strip_ansi(&String::from_utf8_lossy(&self.raw));
            let shown = String::from_utf8_lossy(needle).into_owned();
            self.pump(deadline, timeout, &shown, &text)?;
        }
    }

    /// Reads whatever the child writes during `window` and returns the
    /// stripped transcript so far. For settling after an action whose
    /// completion has no marker of its own.
    pub fn read_for(&mut self, window: std::time::Duration) -> String {
        let deadline = std::time::Instant::now() + window;
        loop {
            let text = strip_ansi(&String::from_utf8_lossy(&self.raw));
            if std::time::Instant::now() >= deadline {
                return text;
            }
            if self
                .pump(deadline, window, "the settle window", &text)
                .is_err()
            {
                return strip_ansi(&String::from_utf8_lossy(&self.raw));
            }
        }
    }

    /// Every byte the child has written to the terminal so far, escapes
    /// included.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// One poll-and-read step; errors with the stripped transcript when
    /// the deadline passes or the slave side hangs up. `needle` only names
    /// what the caller was waiting for in that message.
    fn pump(
        &mut self,
        deadline: std::time::Instant,
        timeout: std::time::Duration,
        needle: &str,
        text: &str,
    ) -> std::io::Result<()> {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        let mut buf = [0u8; 4096];
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{needle:?} did not appear within {timeout:?}:\n{text}"),
            ));
        }
        let mut pfd = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let wait_ms = libc::c_int::try_from(remaining.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: `pfd` is a valid one-element array for poll to fill.
        let ready = unsafe { libc::poll(&mut pfd, 1, wait_ms) };
        if ready == -1 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(err);
        }
        if ready == 0 {
            return Ok(());
        }
        match self.master.read(&mut buf) {
            Ok(0) => Err(closed_early(needle, text)),
            Ok(n) => {
                self.raw.extend_from_slice(&buf[..n]);
                Ok(())
            }
            // Linux reports a hung-up slave as EIO rather than EOF.
            Err(err) if err.raw_os_error() == Some(libc::EIO) => Err(closed_early(needle, text)),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.master.write_all(bytes)?;
        self.master.flush()
    }
}

/// Killing the wrapper is not enough on its own: the session leader's
/// exit drains pending tty output, and the child it leaves behind writes
/// its own goodbye on SIGHUP, so a blocking `wait` with nobody reading
/// the master deadlocks the test after a mid-stream failure. Drain while
/// reaping, and give up after a bounded wait rather than hang the suite.
#[cfg(unix)]
impl Drop for Pty {
    fn drop(&mut self) {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        let _ = self.child.kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut buf = [0u8; 4096];
        let mut draining = true;
        loop {
            match self.child.try_wait() {
                Ok(None) if std::time::Instant::now() < deadline => {}
                _ => break,
            }
            if !draining {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            let mut pfd = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is a valid one-element array for poll to fill.
            let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
            if ready > 0 {
                match self.master.read(&mut buf) {
                    Ok(0) => draining = false,
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => draining = false,
                }
            }
        }
        let _ = self.child.try_wait();
    }
}

#[cfg(unix)]
fn closed_early(needle: &str, text: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("the pty closed before {needle:?} appeared:\n{text}"),
    )
}

#[cfg(unix)]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Drops CSI, OSC and two-byte escape sequences, leaving the printable
/// text. A CSI is parameter bytes, then intermediate bytes, then one final
/// byte; a non-CSI escape may carry one intermediate before its final.
#[cfg(unix)]
pub fn strip_ansi(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\x1b' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('[') => {
                i += 2;
                while matches!(chars.get(i), Some(c) if ('\x30'..='\x3f').contains(c)) {
                    i += 1;
                }
                while matches!(chars.get(i), Some(c) if ('\x20'..='\x2f').contains(c)) {
                    i += 1;
                }
                i += 1;
            }
            Some(']') => {
                i += 2;
                loop {
                    match chars.get(i) {
                        None | Some('\x07') => {
                            i += 1;
                            break;
                        }
                        Some('\x1b') if chars.get(i + 1) == Some(&'\\') => {
                            i += 2;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
            }
            Some(c) if ('\x20'..='\x2f').contains(c) => i += 3,
            Some(_) => i += 2,
            None => i += 1,
        }
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Pty, strip_ansi};
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn csi_with_intermediate_bytes_is_dropped_whole() {
        // Kitty keyboard pop (`ESC [ < u`) and a DEC private mode reset
        // (`ESC [ ? 2004 l`), both with a space intermediate variant.
        assert_eq!(strip_ansi("a\x1b[<ub"), "ab");
        assert_eq!(strip_ansi("a\x1b[?2004lb"), "ab");
        assert_eq!(strip_ansi("a\x1b[2 qb"), "ab");
        assert_eq!(strip_ansi("a\x1b[1;31mb\x1b[0m"), "ab");
    }

    #[test]
    fn non_csi_escapes_with_an_intermediate_are_dropped_whole() {
        assert_eq!(strip_ansi("x\x1b(By"), "xy");
        assert_eq!(strip_ansi("x\x1b=y\x1b>z"), "xyz");
        assert_eq!(strip_ansi("x\x1b]0;title\x07y\x1b]8;;\x1b\\z"), "xyz");
    }

    #[test]
    fn a_truncated_escape_at_the_end_does_not_panic() {
        assert_eq!(strip_ansi("tail\x1b"), "tail");
        assert_eq!(strip_ansi("tail\x1b["), "tail");
        assert_eq!(strip_ansi("tail\x1b]"), "tail");
    }

    /// Neither side of the pty leaks into the child: it sees exactly the
    /// descriptors a child spawned without a pty sees (its three standard
    /// streams, `ls`'s own handle on the directory it lists, and whatever
    /// the test runner itself was started with). On a tty `ls` prints its
    /// entries in columns, so the list is read word by word.
    #[test]
    fn the_child_inherits_no_pty_descriptor() {
        fn listed_fds(text: &str) -> std::collections::BTreeSet<u32> {
            text.split_whitespace()
                .take_while(|word| *word != "fd-list-done")
                .map(|word| word.parse().unwrap())
                .collect()
        }
        let script = "ls /dev/fd; echo fd-list-done";
        let plain = Command::new("sh").arg("-c").arg(script).output().unwrap();
        let baseline = listed_fds(&String::from_utf8_lossy(&plain.stdout));
        assert!(baseline.contains(&0), "{baseline:?}");

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let mut pty = Pty::spawn(cmd).unwrap();
        let transcript = pty
            .wait_for("fd-list-done", Duration::from_secs(10))
            .unwrap();
        let on_pty = listed_fds(&transcript);
        let leaked: Vec<&u32> = on_pty.difference(&baseline).collect();
        assert!(
            leaked.is_empty(),
            "fds {leaked:?} leaked into the pty child (baseline {baseline:?}):\n{transcript}"
        );
    }
}
