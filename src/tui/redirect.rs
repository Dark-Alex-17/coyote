//! Captures what a command turn prints. REPL command handlers write to
//! stdout and stderr by name, so while one runs the UI points the process
//! standard outputs at a pipe, drains it on a thread, and turns the bytes
//! into one transcript cell when they are back on the terminal. On unix
//! the swap is of fds 1 and 2; on Windows it is of the process std
//! handles, which Rust's stdio and children spawned with `Stdio::inherit()`
//! follow while C code writing through the CRT's cached fd 1 does not.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use self::unix::{PipeEnd, SavedStd, pipe, point_std_at};
#[cfg(windows)]
use self::windows::{PipeEnd, SavedStd, pipe, point_std_at};

/// Longest `finish` waits for the drain thread to reach EOF or go quiet.
pub(crate) const SETTLE_LIMIT: Duration = Duration::from_millis(50);
/// Gap without new bytes, once the drain thread has read since the write
/// end closed, after which the pipe counts as quiescent.
pub(crate) const QUIET_GAP: Duration = Duration::from_millis(10);

/// Most of a command's output kept for its cell; a runaway writer is
/// cut here and the tail is counted rather than stored.
const CAPTURE_LIMIT: usize = 1024 * 1024;

static CAPTURED: AtomicBool = AtomicBool::new(false);

/// Whether the standard outputs point into a guard's pipe right now. The
/// panic hook asks: a message the default hook prints there dies with the
/// process.
#[cfg(any(unix, test))]
pub(crate) fn output_captured() -> bool {
    CAPTURED.load(Ordering::SeqCst)
}

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

/// The standard outputs redirected into a pipe for the guard's lifetime.
///
/// Rust's stdout is flushed before each swap so a half-written line is
/// not released onto whichever target is current afterwards. The drain
/// thread is never joined: a child the command spawned (an MCP server
/// whose stderr is inherited) can hold the write end for the whole
/// session, so `finish` waits for EOF only up to a bound, snapshots
/// what has arrived, and leaves the thread to forward anything later.
/// A guard dropped without `finish` (the turn's future was cancelled
/// or panicked) still puts the outputs back and hands what the pipe holds
/// to the late-output callback, so a panic message printed into the pipe
/// is not lost with the cell it would have gone to.
pub(crate) struct FdRedirect {
    saved: Option<SavedStd>,
    write_end: Option<PipeEnd>,
    shared: Arc<Shared>,
}

fn flush_std_streams() {
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

fn drain(read_end: PipeEnd, shared: Arc<Shared>, first_read_delay: Option<Duration>) {
    let mut reader = std::fs::File::from(read_end);
    let mut chunk = [0u8; 4096];
    if let Some(delay) = first_read_delay {
        std::thread::sleep(delay);
    }
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
    /// Points the standard outputs at a fresh pipe. `late` receives output
    /// that arrives after `finish` from a writer still holding the pipe,
    /// and everything buffered when the guard is dropped unfinished.
    pub(crate) fn begin(late: impl Fn(String) + Send + Sync + 'static) -> io::Result<Self> {
        Self::begin_with(late, None)
    }

    /// `begin` with a drain thread that sleeps before its first read,
    /// standing in for a scheduler that has not run it yet.
    #[cfg(all(test, unix))]
    fn begin_with_slow_drain(
        late: impl Fn(String) + Send + Sync + 'static,
        first_read_delay: Duration,
    ) -> io::Result<Self> {
        Self::begin_with(late, Some(first_read_delay))
    }

    fn begin_with(
        late: impl Fn(String) + Send + Sync + 'static,
        first_read_delay: Option<Duration>,
    ) -> io::Result<Self> {
        flush_std_streams();
        let (read_end, write_end) = pipe()?;
        let saved = SavedStd::save()?;
        if let Err(err) = point_std_at(&write_end) {
            Self::restore_saved(&saved);
            return Err(err);
        }
        CAPTURED.store(true, Ordering::SeqCst);
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
            .spawn(move || drain(read_end, thread_shared, first_read_delay))
        {
            Self::restore_saved(&saved);
            return Err(err);
        }
        Ok(Self {
            saved: Some(saved),
            write_end: Some(write_end),
            shared,
        })
    }

    /// Puts the standard outputs back and returns what the pipe received
    /// so far. The write end is closed first, so the drain thread reaches
    /// EOF as soon as it runs unless a child still holds the pipe; the
    /// quiet-gap rule only applies once it has read since then, so a
    /// thread the scheduler has not run yet is waited for, not given
    /// up on.
    pub(crate) fn finish(mut self) -> String {
        self.restore_std();
        let shared = Arc::clone(&self.shared);

        let started = Instant::now();
        let limit = started + SETTLE_LIMIT;
        let mut drained = shared.drained.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let now = Instant::now();
            let deadline = match drained.last_read.filter(|read| *read >= started) {
                Some(read) => (read + QUIET_GAP).min(limit),
                None => limit,
            };
            if drained.eof || now >= deadline {
                break;
            }
            drained = shared
                .changed
                .wait_timeout(drained, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        drained.detached = true;
        drained.take_text()
    }

    /// Restores the standard outputs and closes this side of the pipe; the
    /// drain thread sees EOF once no child holds the write end.
    fn restore_std(&mut self) {
        if let Some(saved) = self.saved.take() {
            Self::restore_saved(&saved);
        }
        self.write_end.take();
    }

    fn restore_saved(saved: &SavedStd) {
        flush_std_streams();
        saved.restore();
        // Cleared once the outputs are back, the mirror of `begin_with`
        // setting it once they point at the pipe.
        CAPTURED.store(false, Ordering::SeqCst);
    }
}

impl Drop for FdRedirect {
    fn drop(&mut self) {
        self.restore_std();
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

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    pub(super) type PipeEnd = OwnedFd;

    /// Both ends close-on-exec from birth, so no child forked meanwhile
    /// can inherit the capture pipe.
    pub(super) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let (read_end, write_end) = io::pipe()?;
        Ok((read_end.into(), write_end.into()))
    }

    pub(super) struct SavedStd {
        stdout: OwnedFd,
        stderr: OwnedFd,
    }

    impl SavedStd {
        pub(super) fn save() -> io::Result<Self> {
            Ok(Self {
                stdout: dup_cloexec(libc::STDOUT_FILENO)?,
                stderr: dup_cloexec(libc::STDERR_FILENO)?,
            })
        }

        pub(super) fn restore(&self) {
            if let Err(err) = dup2(self.stdout.as_raw_fd(), libc::STDOUT_FILENO) {
                warn!("Failed to restore stdout after a command turn: {err}");
            }
            if let Err(err) = dup2(self.stderr.as_raw_fd(), libc::STDERR_FILENO) {
                warn!("Failed to restore stderr after a command turn: {err}");
            }
        }
    }

    pub(super) fn point_std_at(write_end: &OwnedFd) -> io::Result<()> {
        dup2(write_end.as_raw_fd(), libc::STDOUT_FILENO)?;
        dup2(write_end.as_raw_fd(), libc::STDERR_FILENO)
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

    fn dup2(from: i32, to: i32) -> io::Result<()> {
        // SAFETY: both descriptors are open; dup2 takes no pointers.
        if unsafe { libc::dup2(from, to) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;

    pub(super) type PipeEnd = OwnedHandle;

    /// Neither end is inheritable: a child reaches the pipe only through
    /// the std handles, which `Command` duplicates for it at spawn.
    pub(super) fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let mut read_end = ptr::null_mut();
        let mut write_end = ptr::null_mut();
        // SAFETY: both out pointers are valid locals; null attributes ask
        // for the default, non-inheritable handles.
        if unsafe { CreatePipe(&mut read_end, &mut write_end, ptr::null(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both handles were just created and nothing else owns them.
        Ok(unsafe {
            (
                OwnedHandle::from_raw_handle(read_end),
                OwnedHandle::from_raw_handle(write_end),
            )
        })
    }

    /// The std handles as they were before the swap. They belong to the
    /// process, not to the guard, so nothing here closes them.
    pub(super) struct SavedStd {
        stdout: HANDLE,
        stderr: HANDLE,
    }

    // SAFETY: a HANDLE is a process-wide identifier that no thread owns;
    // the guard only passes the values back to SetStdHandle.
    unsafe impl Send for SavedStd {}

    impl SavedStd {
        pub(super) fn save() -> io::Result<Self> {
            Ok(Self {
                stdout: std_handle(STD_OUTPUT_HANDLE)?,
                stderr: std_handle(STD_ERROR_HANDLE)?,
            })
        }

        pub(super) fn restore(&self) {
            if let Err(err) = set_std_handle(STD_OUTPUT_HANDLE, self.stdout) {
                warn!("Failed to restore stdout after a command turn: {err}");
            }
            if let Err(err) = set_std_handle(STD_ERROR_HANDLE, self.stderr) {
                warn!("Failed to restore stderr after a command turn: {err}");
            }
        }
    }

    pub(super) fn point_std_at(write_end: &OwnedHandle) -> io::Result<()> {
        set_std_handle(STD_OUTPUT_HANDLE, write_end.as_raw_handle())?;
        set_std_handle(STD_ERROR_HANDLE, write_end.as_raw_handle())
    }

    fn std_handle(which: STD_HANDLE) -> io::Result<HANDLE> {
        // SAFETY: GetStdHandle takes no pointers; the result is checked.
        let handle = unsafe { GetStdHandle(which) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }

    fn set_std_handle(which: STD_HANDLE, handle: HANDLE) -> io::Result<()> {
        // SAFETY: SetStdHandle stores the value and dereferences nothing.
        if unsafe { SetStdHandle(which, handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::{SavedStdout, capture_stdout, read_all, write_fd1};
    use serial_test::serial;
    use std::fs::File;
    use std::process::Command;
    use std::sync::mpsc;

    fn write_fd2(bytes: &[u8]) {
        // SAFETY: `bytes` is a valid buffer of the given length.
        let n = unsafe { libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n, bytes.len() as isize);
    }

    /// The harness reports from other threads on fd 1 too: the outer
    /// capture holds them back while the guard is live, and whatever
    /// it collected, minus the test's own marker, is passed through to
    /// the real fd 1 afterwards.
    fn release_harness_output(saved: SavedStdout, mut reader: File, own_marker: &str) -> String {
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

    /// The panic hook's view of the capture follows the guard exactly,
    /// whether it ends through `finish` or a drop.
    #[test]
    #[serial]
    fn the_capture_flag_tracks_the_guard() {
        let (saved, reader) = capture_stdout();
        assert!(!output_captured());
        let guard = FdRedirect::begin(|_| {}).unwrap();
        assert!(output_captured());
        guard.finish();
        assert!(!output_captured());
        drop(FdRedirect::begin(|_| {}).unwrap());
        assert!(!output_captured());
        release_harness_output(saved, reader, "");
    }

    /// A drain thread the scheduler has not run yet is waited for:
    /// output written before `finish` lands in the cell even when the
    /// first read comes after the quiet gap would have expired.
    #[test]
    #[serial]
    fn finish_waits_for_a_drain_thread_that_has_not_read_yet() {
        let (saved, reader) = capture_stdout();
        let (tx, rx) = mpsc::channel();
        let guard = FdRedirect::begin_with_slow_drain(
            move |text| {
                let _ = tx.send(text);
            },
            QUIET_GAP * 3,
        )
        .unwrap();
        write_fd1(b"slow-drain-r6x\n");
        let captured = guard.finish();
        release_harness_output(saved, reader, "");
        assert_eq!(captured.trim(), "slow-drain-r6x");
        assert!(rx.try_recv().is_err(), "the output went out as late output");
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use serial_test::serial;
    use std::process::Command;
    use std::sync::mpsc;

    fn write_std_streams(marker: &str) {
        io::stdout()
            .write_all(format!("stdout-{marker}\n").as_bytes())
            .unwrap();
        io::stdout().flush().unwrap();
        io::stderr()
            .write_all(format!("stderr-{marker}\n").as_bytes())
            .unwrap();
        io::stderr().flush().unwrap();
    }

    /// Rust's std streams look the std handles up on every write, so
    /// both markers land in the cell. The test writes through the stream
    /// handles directly because the harness captures the macros first.
    #[test]
    #[serial]
    fn region_output_lands_in_the_cell() {
        let guard = FdRedirect::begin(|_| {}).unwrap();
        write_std_streams("marker-w1x");
        let captured = guard.finish();
        assert!(captured.contains("stdout-marker-w1x"), "{captured}");
        assert!(captured.contains("stderr-marker-w1x"), "{captured}");
    }

    /// The panic hook's view of the capture follows the guard exactly,
    /// whether it ends through `finish` or a drop.
    #[test]
    #[serial]
    fn the_capture_flag_tracks_the_guard() {
        assert!(!output_captured());
        let guard = FdRedirect::begin(|_| {}).unwrap();
        assert!(output_captured());
        guard.finish();
        assert!(!output_captured());
        drop(FdRedirect::begin(|_| {}).unwrap());
        assert!(!output_captured());
    }

    /// What the pipe already holds when an unfinished guard is dropped
    /// goes to the late callback rather than away with the guard.
    #[test]
    #[serial]
    fn dropping_an_unfinished_guard_forwards_what_it_buffered() {
        let (tx, rx) = mpsc::channel();
        let guard = FdRedirect::begin(move |text| {
            let _ = tx.send(text);
        })
        .unwrap();
        write_std_streams("buffered-w5x");
        // Give the drain thread time to move the bytes into the buffer,
        // so it is the drop that forwards them.
        std::thread::sleep(Duration::from_millis(100));
        drop(guard);
        let late = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(late.contains("stdout-buffered-w5x"), "{late}");
    }

    /// A child spawned with inherited stdio gets the swapped std handles,
    /// so what it prints is part of the cell.
    #[test]
    #[serial]
    fn a_child_with_inherited_stdio_writes_into_the_cell() {
        let guard = FdRedirect::begin(|_| {}).unwrap();
        Command::new("cmd")
            .args(["/c", "echo child-marker-x7x"])
            .status()
            .unwrap();
        let captured = guard.finish();
        assert!(captured.contains("child-marker-x7x"), "{captured}");
    }
}
