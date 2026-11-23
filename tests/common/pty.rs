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
/// all three standard streams; the test holds the master side. Like a
/// terminal without the kitty keyboard protocol, it answers crossterm's
/// keyboard-enhancement probe with device attributes alone, unless the
/// test opts out.
#[cfg(unix)]
pub struct Pty {
    master: std::fs::File,
    child: std::process::Child,
    raw: Vec<u8>,
    answers_keyboard_probe: bool,
    keyboard_probe_answered: usize,
}

/// crossterm's keyboard-enhancement probe: the flags query, then a
/// primary device-attributes query as the sentinel.
#[cfg(unix)]
const KEYBOARD_PROBE: &[u8] = b"\x1b[?u\x1b[c";
/// A VT220-class terminal's device attributes, with no flags reply ahead
/// of it: crossterm reads that as no keyboard-enhancement support.
#[cfg(unix)]
const KEYBOARD_PROBE_REPLY: &[u8] = b"\x1b[?62;22c";

#[cfg(unix)]
impl Pty {
    /// Spawns on a 100x40 terminal, the size the fixture `tput` reports.
    pub fn spawn(mut cmd: Command) -> std::io::Result<Pty> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let (master, slave) = open_pair(40, 100)?;
        let slave_fd = slave.as_raw_fd();
        // SAFETY: the hook calls only libc functions that are plain
        // syscalls or async-signal-safe (signal, getrlimit included, which
        // sysconf is not on every libc) on descriptors that stay open in
        // the parent until after spawn returns.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // A runner started in the background by a non-interactive
                // shell inherits SIGINT and SIGQUIT ignored, one under nohup
                // inherits SIGHUP ignored, and an ignored signal survives
                // exec: a shell cannot even trap it. The child leads a
                // terminal session, so it gets the dispositions a terminal's
                // session leader starts with.
                for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                    if libc::signal(sig, libc::SIG_DFL) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
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
                // Everything else the test runner holds without
                // close-on-exec goes too, scanning the descriptors below
                // 1024 (or the soft limit when lower): a child must never
                // inherit a lock file or a sibling test's pipe. Descriptors
                // that carry the flag are left alone: exec drops them
                // anyway, and one of them is std's exec-status pipe, which
                // the parent needs to learn that execvp failed.
                let mut nofile = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                let limit = match libc::getrlimit(libc::RLIMIT_NOFILE, &mut nofile) {
                    0 if nofile.rlim_cur != libc::RLIM_INFINITY => nofile.rlim_cur.min(1024) as i32,
                    _ => 1024,
                };
                for fd in 3..limit {
                    if fd == slave_fd {
                        continue;
                    }
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                        libc::close(fd);
                    }
                }
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
            answers_keyboard_probe: true,
            keyboard_probe_answered: 0,
        })
    }

    /// Stops the automatic reply to the keyboard-enhancement probe, for a
    /// test that answers it itself or needs it still waiting. Call it
    /// before the first wait.
    pub fn leave_keyboard_probe_unanswered(&mut self) {
        self.answers_keyboard_probe = false;
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

    /// Like `wait_for`, but only text the child wrote at or after raw
    /// offset `from` counts, so a needle that already appeared earlier in
    /// the session (the footer's "no session", say) has to appear again.
    pub fn wait_for_after(
        &mut self,
        needle: &str,
        from: usize,
        timeout: std::time::Duration,
    ) -> std::io::Result<String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let since = strip_ansi(&String::from_utf8_lossy(
                &self.raw[from.min(self.raw.len())..],
            ));
            if since.contains(needle) {
                return Ok(since);
            }
            let text = strip_ansi(&String::from_utf8_lossy(&self.raw));
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

    /// The master side's descriptor, for checks on its flags.
    pub fn master_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.master.as_raw_fd()
    }

    /// One poll-and-read step, answering any keyboard-enhancement probe it
    /// read; errors with the stripped transcript when the deadline passes
    /// or the slave side hangs up. `needle` only names what the caller was
    /// waiting for in that message.
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
                self.answer_keyboard_probe()
            }
            // Linux reports a hung-up slave as EIO rather than EOF.
            Err(err) if err.raw_os_error() == Some(libc::EIO) => Err(closed_early(needle, text)),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn answer_keyboard_probe(&mut self) -> std::io::Result<()> {
        if !self.answers_keyboard_probe {
            return Ok(());
        }
        while let Some(at) = find_bytes(&self.raw[self.keyboard_probe_answered..], KEYBOARD_PROBE) {
            self.keyboard_probe_answered += at + KEYBOARD_PROBE.len();
            self.send(KEYBOARD_PROBE_REPLY)?;
        }
        Ok(())
    }

    pub fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.master.write_all(bytes)?;
        self.master.flush()
    }

    /// Changes the terminal's size, which the kernel reports to the child's
    /// foreground process group as SIGWINCH, the way a user dragging the
    /// window edge does.
    pub fn resize(&mut self, rows: u16, cols: u16) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads one winsize through a valid pointer.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// The terminal's current height in rows.
    pub fn rows(&self) -> std::io::Result<u16> {
        use std::os::fd::AsRawFd;
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ writes one winsize through a valid pointer.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCGWINSZ as _, &mut size) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(size.ws_row)
    }

    /// Whether the terminal echoes input. Termios requests on the master
    /// act on the slave's line discipline, so this is what the child sees.
    pub fn echoes(&self) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        // SAFETY: termios is plain integers and arrays; all zeroes is a
        // valid value for tcgetattr to overwrite.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: tcgetattr writes one termios through a valid pointer.
        if unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut termios) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(termios.c_lflag & libc::ECHO != 0)
    }

    /// The `ps` rows of every process in the child's group and the
    /// terminal's `stty -a` report, for a failure message about a child
    /// that stopped responding. The child leads its own session, so its
    /// pid is the group id its descendants inherit.
    pub fn diagnostics(&self) -> String {
        let group = self.child.id().to_string();
        let ps = match Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,command="])
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|row| row.split_whitespace().nth(2) == Some(group.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(err) => format!("ps failed: {err}"),
        };
        let stty = match self
            .master
            .try_clone()
            .and_then(|master| Command::new("stty").arg("-a").stdin(master).output())
        {
            Ok(out) => format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(err) => format!("stty failed: {err}"),
        };
        format!(
            "ps (pid ppid pgid stat command):\n{ps}\nstty -a:\n{}",
            stty.trim_end()
        )
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

        let child_pgrp = self.child.id() as i32;
        // Under the job-control wrapper the binary runs in its own
        // foreground group, which the wrapper's death would orphan. The
        // foreground group and its session are read BEFORE the child is
        // killed: once the session leader is gone the tty has no
        // foreground group to report and getsid has nothing to answer.
        // The group is only killed while it still belongs to the child's
        // session (the child is its leader, so the session id is its pid):
        // a stale id may already name an unrelated process group.
        // SAFETY: tcgetpgrp reads one int from the tty behind an open fd;
        // getsid takes a pid and touches no memory.
        let foreground = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        let foreground_is_ours = foreground > 1
            && foreground != child_pgrp
            && unsafe { libc::getsid(foreground) } == child_pgrp;
        let _ = self.child.kill();
        if foreground_is_ours {
            // SAFETY: kill takes a pid and a signal number and touches no memory.
            unsafe { libc::kill(-foreground, libc::SIGKILL) };
        }
        // The child is a session leader (setsid in pre_exec), so its pid
        // names the process group; a coyote grandchild under a shell
        // wrapper goes with it instead of being orphaned.
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        unsafe { libc::kill(-child_pgrp, libc::SIGKILL) };
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

/// Opens a master/slave pair of the given size with close-on-exec set on
/// both, so neither leaks into a child's descriptor table past the dup2
/// that `spawn` does for the standard streams.
#[cfg(unix)]
pub fn open_pair(
    rows: u16,
    cols: u16,
) -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: both out-pointers are valid ints, `size` is a valid
    // winsize; name and termios may be null. `size` goes as a raw
    // pointer: the parameter is `*const` on Linux and `*mut` on the BSDs,
    // and a raw `*mut` fits both.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openpty just created both descriptors and nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: fcntl with F_SETFD takes no pointers; the result is checked.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((master, slave))
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
                // Only a real final byte is consumed: a CSI cut short by a
                // bare ESC leaves that ESC to start the next sequence.
                if matches!(chars.get(i), Some(c) if ('\x40'..='\x7e').contains(c)) {
                    i += 1;
                }
            }
            // OSC, DCS, APC, PM and SOS strings run to ST or BEL; their
            // payload is not screen text.
            Some(']' | 'P' | '_' | '^' | 'X') => {
                i += 2;
                // A bare ESC ends the string too, as on a terminal, and the
                // sequence it starts is handled by the next round.
                loop {
                    match chars.get(i) {
                        None | Some('\x07') => {
                            i += 1;
                            break;
                        }
                        Some('\x1b') => {
                            if chars.get(i + 1) == Some(&'\\') {
                                i += 2;
                            }
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
