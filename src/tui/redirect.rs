//! Captures what a command turn prints. REPL command handlers write to
//! stdout and stderr by name, so while one runs the UI points fds 1 and 2
//! at a pipe, drains it on a thread, and turns the bytes into one
//! transcript cell when the fds are back on the terminal.

#[cfg(not(unix))]
pub(crate) use other::FdRedirect;
#[cfg(unix)]
pub(crate) use unix::FdRedirect;

use std::time::Duration;

/// Longest `finish` waits for the drain thread to catch up.
pub(crate) const SETTLE_LIMIT: Duration = Duration::from_millis(50);
/// Gap without new bytes after which the pipe counts as quiescent.
pub(crate) const QUIET_GAP: Duration = Duration::from_millis(10);

/// Command output stays on the console until the Windows twin lands.
#[cfg(not(unix))]
mod other {
    pub(crate) struct FdRedirect;

    impl FdRedirect {
        pub(crate) fn begin(
            _late: impl Fn(String) + Send + Sync + 'static,
        ) -> std::io::Result<Self> {
            Ok(Self)
        }

        pub(crate) fn finish(self) -> String {
            String::new()
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::{QUIET_GAP, SETTLE_LIMIT};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    /// Most of a command's output kept for its cell; a runaway writer is
    /// cut here and the tail is counted rather than stored.
    const CAPTURE_LIMIT: usize = 1024 * 1024;

    struct Drained {
        buf: Vec<u8>,
        dropped: usize,
        last_read: Option<Instant>,
        eof: bool,
        /// Set by `finish`: bytes read from here on belong to no cell and
        /// go through the late-output callback instead.
        detached: bool,
    }

    impl Drained {
        fn push(&mut self, chunk: &[u8]) {
            let room = CAPTURE_LIMIT.saturating_sub(self.buf.len());
            let kept = chunk.len().min(room);
            self.buf.extend_from_slice(&chunk[..kept]);
            self.dropped += chunk.len() - kept;
        }

        fn take_text(&mut self) -> String {
            let mut text = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            if self.dropped > 0 {
                text.push_str(&format!("\n... {} bytes dropped\n", self.dropped));
                self.dropped = 0;
            }
            text
        }
    }

    struct Shared {
        drained: Mutex<Drained>,
        changed: Condvar,
        late: Box<dyn Fn(String) + Send + Sync>,
    }

    /// Fds 1 and 2 redirected into a pipe for the guard's lifetime.
    ///
    /// Rust's stdout is flushed before each swap so a half-written line is
    /// not released onto whichever fd is current afterwards. The drain
    /// thread is never joined: a child the command spawned (an MCP server
    /// whose stderr is inherited) can hold the write end for the whole
    /// session, so `finish` waits only for quiescence, snapshots what has
    /// arrived, and leaves the thread to forward anything later. A guard
    /// dropped without `finish` (the turn's future was cancelled or
    /// panicked) still puts the fds back and hands what the pipe holds to
    /// the late-output callback, so a panic message printed into the pipe
    /// is not lost with the cell it would have gone to.
    pub(crate) struct FdRedirect {
        saved: Option<SavedFds>,
        write_end: Option<OwnedFd>,
        shared: Arc<Shared>,
    }

    struct SavedFds {
        stdout: OwnedFd,
        stderr: OwnedFd,
    }

    impl SavedFds {
        fn save() -> io::Result<Self> {
            Ok(Self {
                stdout: dup_cloexec(libc::STDOUT_FILENO)?,
                stderr: dup_cloexec(libc::STDERR_FILENO)?,
            })
        }

        fn restore(&self) {
            flush_std_streams();
            if let Err(err) = dup2(self.stdout.as_raw_fd(), libc::STDOUT_FILENO) {
                warn!("Failed to restore stdout after a command turn: {err}");
            }
            if let Err(err) = dup2(self.stderr.as_raw_fd(), libc::STDERR_FILENO) {
                warn!("Failed to restore stderr after a command turn: {err}");
            }
        }
    }

    fn dup_cloexec(fd: i32) -> io::Result<OwnedFd> {
        // SAFETY: fcntl with F_DUPFD_CLOEXEC takes no pointers; the result is checked.
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` was just created by fcntl and nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(dup) })
    }

    /// Both ends close-on-exec from birth, so no child forked meanwhile
    /// can inherit the capture pipe.
    fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let (read_end, write_end) = io::pipe()?;
        Ok((read_end.into(), write_end.into()))
    }

    fn dup2(from: i32, to: i32) -> io::Result<()> {
        // SAFETY: both descriptors are open; dup2 takes no pointers.
        if unsafe { libc::dup2(from, to) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn flush_std_streams() {
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
    }

    fn drain(read_end: OwnedFd, shared: Arc<Shared>) {
        let mut reader = std::fs::File::from(read_end);
        let mut chunk = [0u8; 4096];
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let forward = {
                let mut drained = shared.drained.lock().unwrap_or_else(|p| p.into_inner());
                if drained.detached {
                    Some(String::from_utf8_lossy(&chunk[..n]).into_owned())
                } else {
                    drained.push(&chunk[..n]);
                    drained.last_read = Some(Instant::now());
                    None
                }
            };
            shared.changed.notify_all();
            if let Some(text) = forward {
                (shared.late)(text);
            }
        }
        shared.drained.lock().unwrap_or_else(|p| p.into_inner()).eof = true;
        shared.changed.notify_all();
    }

    impl FdRedirect {
        /// Points fds 1 and 2 at a fresh pipe. `late` receives output that
        /// arrives after `finish` from a writer still holding the pipe, and
        /// everything buffered when the guard is dropped unfinished.
        pub(crate) fn begin(late: impl Fn(String) + Send + Sync + 'static) -> io::Result<Self> {
            flush_std_streams();
            let (read_end, write_end) = pipe()?;
            let saved = SavedFds::save()?;
            dup2(write_end.as_raw_fd(), libc::STDOUT_FILENO)?;
            if let Err(err) = dup2(write_end.as_raw_fd(), libc::STDERR_FILENO) {
                saved.restore();
                return Err(err);
            }
            let shared = Arc::new(Shared {
                drained: Mutex::new(Drained {
                    buf: Vec::new(),
                    dropped: 0,
                    last_read: None,
                    eof: false,
                    detached: false,
                }),
                changed: Condvar::new(),
                late: Box::new(late),
            });
            let thread_shared = Arc::clone(&shared);
            if let Err(err) = std::thread::Builder::new()
                .name("tui-fd-drain".into())
                .spawn(move || drain(read_end, thread_shared))
            {
                saved.restore();
                return Err(err);
            }
            Ok(Self {
                saved: Some(saved),
                write_end: Some(write_end),
                shared,
            })
        }

        /// Puts fds 1 and 2 back and returns what the pipe received so far.
        pub(crate) fn finish(mut self) -> String {
            self.restore_fds();
            let shared = Arc::clone(&self.shared);

            let started = Instant::now();
            let mut drained = shared.drained.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                let now = Instant::now();
                let quiet_since = drained.last_read.map_or(started, |t| t.max(started));
                if drained.eof
                    || now.duration_since(quiet_since) >= QUIET_GAP
                    || now.duration_since(started) >= SETTLE_LIMIT
                {
                    break;
                }
                let wait = (quiet_since + QUIET_GAP).min(started + SETTLE_LIMIT) - now;
                drained = shared
                    .changed
                    .wait_timeout(drained, wait)
                    .unwrap_or_else(|p| p.into_inner())
                    .0;
            }
            drained.detached = true;
            drained.take_text()
        }

        /// Restores fds 1 and 2 and closes this side of the pipe; the
        /// drain thread sees EOF once no child holds the write end.
        fn restore_fds(&mut self) {
            if let Some(saved) = self.saved.take() {
                saved.restore();
            }
            self.write_end.take();
        }
    }

    impl Drop for FdRedirect {
        fn drop(&mut self) {
            self.restore_fds();
            let buffered = {
                let mut drained = self
                    .shared
                    .drained
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                drained.detached = true;
                drained.take_text()
            };
            if !buffered.is_empty() {
                (self.shared.late)(buffered);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::testing::{SavedStdout, capture_stdout, read_all, write_fd1};
        use serial_test::serial;
        use std::fs::File;
        use std::process::Command;
        use std::sync::mpsc;
        use std::time::Duration;

        fn write_fd2(bytes: &[u8]) {
            // SAFETY: `bytes` is a valid buffer of the given length.
            let n = unsafe { libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len()) };
            assert_eq!(n, bytes.len() as isize);
        }

        /// The harness reports from other threads on fd 1 too: the outer
        /// capture holds them back while the guard is live, and whatever
        /// it collected, minus the test's own marker, is passed through to
        /// the real fd 1 afterwards.
        fn release_harness_output(
            saved: SavedStdout,
            mut reader: File,
            own_marker: &str,
        ) -> String {
            drop(saved);
            let outside = read_all(&mut reader);
            write_fd1(outside.replacen(own_marker, "", 1).as_bytes());
            outside
        }

        /// Raw writes to fds 1 and 2 inside the region come back from
        /// `finish`, and the terminal's own stdout (the outer capture pipe
        /// here) sees none of them. The test writes raw because the
        /// harness captures the std stream macros before they reach fd 1.
        #[test]
        #[serial]
        fn region_output_lands_in_the_cell_and_nowhere_else() {
            let (saved, reader) = capture_stdout();
            let guard = FdRedirect::begin(|_| {}).unwrap();
            write_fd1(b"stdout-marker-r1x\n");
            write_fd2(b"stderr-marker-r1x\n");
            let captured = guard.finish();
            write_fd1(b"after-r1x\n");
            let outside = release_harness_output(saved, reader, "after-r1x\n");

            assert!(captured.contains("stdout-marker-r1x"), "{captured}");
            assert!(captured.contains("stderr-marker-r1x"), "{captured}");
            assert!(!outside.contains("-marker-r1x"), "{outside}");
            assert!(outside.contains("after-r1x"), "{outside}");
        }

        /// A child still holding the pipe must not hold up the command
        /// turn: `finish` settles on quiescence, never on EOF.
        #[test]
        #[serial]
        fn finish_never_waits_for_a_lingering_writer() {
            let (saved, reader) = capture_stdout();
            let guard = FdRedirect::begin(|_| {}).unwrap();
            Command::new("sh")
                .arg("-c")
                .arg("echo before-r2x >&2; sleep 5 >&2 &")
                .status()
                .unwrap();
            let started = Instant::now();
            let captured = guard.finish();
            let elapsed = started.elapsed();
            release_harness_output(saved, reader, "");
            assert!(elapsed < Duration::from_secs(2), "finish took {elapsed:?}");
            assert_eq!(captured.trim(), "before-r2x");
        }

        /// Bytes a lingering writer sends after `finish` reach the late
        /// callback instead of vanishing.
        #[test]
        #[serial]
        fn late_output_arrives_through_the_callback() {
            let (saved, reader) = capture_stdout();
            let (tx, rx) = mpsc::channel();
            let guard = FdRedirect::begin(move |text| {
                let _ = tx.send(text);
            })
            .unwrap();
            Command::new("sh")
                .arg("-c")
                .arg("(sleep 0.3; echo late-r3x) >&2 &")
                .status()
                .unwrap();
            let captured = guard.finish();
            release_harness_output(saved, reader, "");
            assert!(!captured.contains("late-r3x"), "{captured}");
            let late = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(late.contains("late-r3x"), "{late}");
        }

        /// A guard that goes away without `finish` (its turn was cancelled)
        /// still hands fd 1 back: a marker written afterwards reaches the
        /// outer capture, not the pipe.
        #[test]
        #[serial]
        fn dropping_an_unfinished_guard_restores_the_fds() {
            let (saved, reader) = capture_stdout();
            let guard = FdRedirect::begin(|_| {}).unwrap();
            write_fd1(b"inside-r4x\n");
            drop(guard);
            write_fd1(b"after-drop-r4x\n");
            let outside = release_harness_output(saved, reader, "after-drop-r4x\n");

            assert!(outside.contains("after-drop-r4x"), "{outside}");
            assert!(!outside.contains("inside-r4x"), "{outside}");
        }

        /// What the pipe already holds when an unfinished guard is dropped
        /// (a panic message, say) goes to the late callback rather than
        /// away with the guard.
        #[test]
        #[serial]
        fn dropping_an_unfinished_guard_forwards_what_it_buffered() {
            let (saved, reader) = capture_stdout();
            let (tx, rx) = mpsc::channel();
            let guard = FdRedirect::begin(move |text| {
                let _ = tx.send(text);
            })
            .unwrap();
            write_fd1(b"buffered-r5x\n");
            // Give the drain thread time to move the bytes into the buffer,
            // so it is the drop that forwards them.
            std::thread::sleep(Duration::from_millis(100));
            drop(guard);
            let outside = release_harness_output(saved, reader, "");

            let late = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(late.contains("buffered-r5x"), "{late}");
            assert!(!outside.contains("buffered-r5x"), "{outside}");
        }

        /// A writer that floods the pipe gets its first mebibyte kept and
        /// the rest counted in one trailing marker.
        #[test]
        #[serial]
        fn output_past_the_capture_limit_is_dropped_and_counted_once() {
            let (saved, reader) = capture_stdout();
            let guard = FdRedirect::begin(|_| {}).unwrap();
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..32 {
                write_fd1(&chunk);
            }
            let captured = guard.finish();
            release_harness_output(saved, reader, "");

            let marker = format!("\n... {} bytes dropped\n", CAPTURE_LIMIT);
            assert_eq!(captured.len(), CAPTURE_LIMIT + marker.len());
            assert!(
                captured.ends_with(&marker),
                "{}",
                &captured[CAPTURE_LIMIT..]
            );
            assert_eq!(captured.matches("bytes dropped").count(), 1);
        }
    }
}
