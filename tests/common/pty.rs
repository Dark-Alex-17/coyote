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
#[allow(dead_code)] // TASK-312
pub struct Pty {
    master: std::fs::File,
    child: std::process::Child,
    raw: Vec<u8>,
}

#[cfg(unix)]
impl Pty {
    #[allow(dead_code)] // TASK-312
    pub fn spawn(mut cmd: Command) -> std::io::Result<Pty> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;

        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        // SAFETY: both out-pointers are valid ints; name, termios and winsize may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
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
    #[allow(dead_code)] // TASK-312
    pub fn wait_for(
        &mut self,
        needle: &str,
        timeout: std::time::Duration,
    ) -> std::io::Result<String> {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        let deadline = std::time::Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        loop {
            let text = strip_ansi(&String::from_utf8_lossy(&self.raw));
            if text.contains(needle) {
                return Ok(text);
            }
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
                    continue;
                }
                return Err(err);
            }
            if ready == 0 {
                continue;
            }
            match self.master.read(&mut buf) {
                Ok(0) => return Err(closed_early(needle, &text)),
                Ok(n) => self.raw.extend_from_slice(&buf[..n]),
                // Linux reports a hung-up slave as EIO rather than EOF.
                Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                    return Err(closed_early(needle, &text));
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    }

    #[allow(dead_code)] // TASK-312
    pub fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.master.write_all(bytes)?;
        self.master.flush()
    }
}

#[cfg(unix)]
impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn closed_early(needle: &str, text: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("the pty closed before {needle:?} appeared:\n{text}"),
    )
}

/// Drops CSI and OSC escape sequences, leaving the printable text.
#[cfg(unix)]
fn strip_ansi(text: &str) -> String {
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
                while matches!(chars.get(i), Some(c) if ('\x20'..='\x3f').contains(c)) {
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
            Some(_) => i += 2,
            None => i += 1,
        }
    }
    out
}
