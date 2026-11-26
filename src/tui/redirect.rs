//! Captures what a command turn prints. REPL command handlers write to
//! stdout and stderr by name, so while one runs the UI points the process
//! standard outputs at a pipe, drains it on a thread, and turns the bytes
//! into one transcript cell when they are back on the terminal. On unix
//! the swap is of fds 1 and 2; on Windows it is of the process std
//! handles, which Rust's stdio and children spawned with `Stdio::inherit()`
//! follow while C code writing through the CRT's cached fd 1 does not.
//!
//! The read end is only read under the state lock: on unix it is
//! non-blocking, and on Windows it is an overlapped named pipe the drain
//! thread watches with a zero-byte read that takes nothing. So `finish`
//! empties the pipe itself and output written before it never depends on
//! when the drain thread runs. Only a child still writing during `finish`
//! can land in either the cell or a late notice.

use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use self::unix::{PipeEnd, SavedStd, pending, pipe, point_std_at, read_some};
#[cfg(windows)]
use self::windows::{PipeEnd, SavedStd, pending, pipe, point_std_at, read_some};

/// Longest `finish` waits for the drain thread to reach EOF or go quiet.
pub(crate) const SETTLE_LIMIT: Duration = Duration::from_millis(50);
/// Gap without new bytes, once the drain thread has read since the write
/// end closed, after which the pipe counts as quiescent.
pub(crate) const QUIET_GAP: Duration = Duration::from_millis(10);

/// Most of a command's output kept for its cell; a runaway writer is
/// cut here and the tail is counted rather than stored.
const CAPTURE_LIMIT: usize = 1024 * 1024;

const READ_CHUNK: usize = 64 * 1024;

static CAPTURED: AtomicBool = AtomicBool::new(false);

/// Whether the standard outputs point into a guard's pipe right now. The
/// panic hook asks: a message the default hook prints there dies with the
/// process.
pub(crate) fn output_captured() -> bool {
    CAPTURED.load(Ordering::SeqCst)
}

enum Got {
    Data(usize),
    Empty,
    Eof,
}

struct Drained {
    /// The pipe's read end. Every read that takes bytes happens under the
    /// lock, so bytes are either still in the pipe or already in `buf`
    /// (or forwarded), never in flight on the drain thread.
    #[cfg(unix)]
    reader: std::fs::File,
    #[cfg(windows)]
    reader: Arc<windows::Reader>,
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

    /// Moves what the pipe holds into `buf`. It stops after the bytes
    /// queued when it began if more keep coming, so a child that never
    /// stops writing cannot keep it here.
    fn sweep(&mut self) {
        let queued = pending(&self.reader).unwrap_or(usize::MAX);
        let mut chunk = vec![0u8; READ_CHUNK];
        let mut swept = 0;
        while !self.eof {
            match read_some(&self.reader, &mut chunk) {
                Ok(Got::Data(n)) => {
                    self.push(&chunk[..n]);
                    swept += n;
                    if swept > queued {
                        break;
                    }
                }
                Ok(Got::Empty) => break,
                Ok(Got::Eof) | Err(_) => self.eof = true,
            }
        }
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
/// session, so after taking what the pipe holds `finish` waits for more
/// only up to a bound and leaves the thread to forward anything later.
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

/// Sync points a test uses to hold the drain thread before its first
/// read and to learn when it has exited.
#[cfg(test)]
#[derive(Default)]
struct DrainHooks {
    gate: Option<std::sync::mpsc::Receiver<()>>,
    exited: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(test)]
impl DrainHooks {
    fn hold_at_gate(&mut self) {
        if let Some(gate) = self.gate.take() {
            let _ = gate.recv();
        }
    }

    fn report_exit(&self) {
        if let Some(exited) = &self.exited {
            let _ = exited.send(());
        }
    }
}

#[cfg(not(test))]
#[derive(Default)]
struct DrainHooks {}

#[cfg(not(test))]
impl DrainHooks {
    fn hold_at_gate(&mut self) {}

    fn report_exit(&self) {}
}

/// Waits for the pipe without the lock and reads it with the lock held,
/// so `finish` may already have taken what woke it.
#[cfg(unix)]
fn drain(fd: RawFd, shared: Arc<Shared>, mut hooks: DrainHooks) {
    // SAFETY: `fd` is the reader inside `shared`, which this thread's Arc
    // keeps open until the function returns.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut chunk = vec![0u8; READ_CHUNK];
    hooks.hold_at_gate();
    loop {
        let ready = unix::wait_readable(fd);
        let forward = {
            let mut drained = shared.drained.lock().unwrap_or_else(|p| p.into_inner());
            if drained.eof {
                break;
            }
            if ready.is_err() {
                drained.eof = true;
                break;
            }
            match unix::read_some(&drained.reader, &mut chunk) {
                Ok(Got::Empty) => None,
                Ok(Got::Eof) | Err(_) => {
                    drained.eof = true;
                    break;
                }
                Ok(Got::Data(n)) if drained.detached => {
                    Some(String::from_utf8_lossy(&chunk[..n]).into_owned())
                }
                Ok(Got::Data(n)) => {
                    drained.push(&chunk[..n]);
                    drained.last_read = Some(Instant::now());
                    None
                }
            }
        };
        shared.changed.notify_all();
        if let Some(text) = forward {
            (shared.late)(text);
        }
    }
    shared.changed.notify_all();
    hooks.report_exit();
}

/// Waits for the pipe without the lock and reads it with the lock held,
/// so `finish` may already have taken what woke it. A failed wait still
/// reads first: the pipe may hold bytes after its last writer closed.
#[cfg(windows)]
fn drain(reader: Arc<windows::Reader>, shared: Arc<Shared>, mut hooks: DrainHooks) {
    let mut chunk = vec![0u8; READ_CHUNK];
    hooks.hold_at_gate();
    loop {
        let ready = windows::wait_readable(&reader);
        let forward = {
            let mut drained = shared.drained.lock().unwrap_or_else(|p| p.into_inner());
            if drained.eof {
                break;
            }
            match windows::read_some(&drained.reader, &mut chunk) {
                Ok(Got::Empty) if ready.is_ok() => None,
                Ok(Got::Empty | Got::Eof) | Err(_) => {
                    drained.eof = true;
                    break;
                }
                Ok(Got::Data(n)) if drained.detached => {
                    Some(String::from_utf8_lossy(&chunk[..n]).into_owned())
                }
                Ok(Got::Data(n)) => {
                    drained.push(&chunk[..n]);
                    drained.last_read = Some(Instant::now());
                    None
                }
            }
        };
        shared.changed.notify_all();
        if let Some(text) = forward {
            (shared.late)(text);
        }
    }
    shared.changed.notify_all();
    hooks.report_exit();
}

impl FdRedirect {
    /// Points the standard outputs at a fresh pipe. `late` receives output
    /// that arrives after `finish` from a writer still holding the pipe,
    /// and everything buffered when the guard is dropped unfinished.
    pub(crate) fn begin(late: impl Fn(String) + Send + Sync + 'static) -> io::Result<Self> {
        Self::begin_with(late, DrainHooks::default())
    }

    fn begin_with(
        late: impl Fn(String) + Send + Sync + 'static,
        hooks: DrainHooks,
    ) -> io::Result<Self> {
        flush_std_streams();
        let (read_end, write_end) = pipe()?;
        // Only the read end: fds 1 and 2 and every child share the write
        // end's open file description and would see EAGAIN.
        #[cfg(unix)]
        unix::set_nonblocking(&read_end)?;
        #[cfg(unix)]
        let reader = std::fs::File::from(read_end);
        #[cfg(unix)]
        let drain_end = reader.as_raw_fd();
        #[cfg(windows)]
        let reader = Arc::new(windows::Reader::new(read_end)?);
        #[cfg(windows)]
        let drain_end = Arc::clone(&reader);
        let saved = SavedStd::save()?;
        if let Err(err) = point_std_at(&write_end) {
            Self::restore_saved(&saved);
            return Err(err);
        }
        CAPTURED.store(true, Ordering::SeqCst);
        let shared = Arc::new(Shared {
            drained: Mutex::new(Drained {
                reader,
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
            .spawn(move || drain(drain_end, thread_shared, hooks))
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
    /// so far. Rust's stdio is flushed and the write end closed first, so
    /// everything written before this call is already queued and is read
    /// here under the lock, whether or not the drain thread has run. Only
    /// a child still holding the pipe gets the grace after that: up to
    /// the quiet gap or the settle limit, its output may land in the cell
    /// or reach the late callback.
    pub(crate) fn finish(mut self) -> String {
        self.restore_std();
        let shared = Arc::clone(&self.shared);

        let mut drained = shared.drained.lock().unwrap_or_else(|p| p.into_inner());
        drained.sweep();
        let started = Instant::now();
        let limit = started + SETTLE_LIMIT;
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
            drained.sweep();
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
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

    use super::Got;

    pub(super) type PipeEnd = OwnedFd;

    /// Both ends close-on-exec from birth, so no child forked meanwhile
    /// can inherit the capture pipe.
    pub(super) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let (read_end, write_end) = io::pipe()?;
        Ok((read_end.into(), write_end.into()))
    }

    pub(super) fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
        // SAFETY: fcntl with F_GETFL takes no pointers; the result is checked.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl with F_SETFL takes an integer flag set, no pointers.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn read_some(mut reader: &File, buf: &mut [u8]) -> io::Result<Got> {
        loop {
            match reader.read(buf) {
                Ok(0) => return Ok(Got::Eof),
                Ok(n) => return Ok(Got::Data(n)),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(Got::Empty),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    }

    /// Bytes queued in the pipe right now.
    pub(super) fn pending(reader: &File) -> io::Result<usize> {
        let mut queued: libc::c_int = 0;
        // SAFETY: FIONREAD writes one c_int through the pointer, which
        // points at a live local of that type.
        if unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD as _, &mut queued) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(usize::try_from(queued).unwrap_or(0))
    }

    /// Blocks until `fd` is readable, at EOF or in error; the read that
    /// follows tells which.
    pub(super) fn wait_readable(fd: BorrowedFd<'_>) -> io::Result<()> {
        let mut entry = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `entry` is one valid pollfd and `fd` is borrowed, so open.
            let ready = unsafe { libc::poll(&mut entry, 1, -1) };
            if ready == -1 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if entry.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            if ready > 0 {
                return Ok(());
            }
        }
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
    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_IO_PENDING, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_WAIT, PeekNamedPipe,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::Got;

    pub(super) type PipeEnd = OwnedHandle;

    const PIPE_BUFFER: u32 = 64 * 1024;

    /// The read end is overlapped so the drain thread can wait on it while
    /// `finish` reads it; the write end is synchronous because children and
    /// Rust's stdio write to it that way. Neither end is inheritable: a
    /// child reaches the pipe only through the std handles, which `Command`
    /// duplicates for it at spawn. The name is random and claimed with
    /// FILE_FLAG_FIRST_PIPE_INSTANCE, so another process cannot take it
    /// first and receive the capture.
    pub(super) fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let name: Vec<u16> = format!(
            r"\\.\pipe\coyote-capture-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        // SAFETY: `name` is NUL terminated and outlives the call; null
        // attributes ask for the default, non-inheritable handle.
        let read_end = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                ptr::null(),
            )
        };
        if read_end == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateNamedPipeW returned a fresh handle nobody else owns.
        let read_end = unsafe { OwnedHandle::from_raw_handle(read_end) };
        // A new instance is already listening, so this open connects it
        // and no ConnectNamedPipe is needed.
        // SAFETY: `name` is NUL terminated and outlives the call; null
        // attributes ask for the default, non-inheritable handle.
        let write_end = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_NONE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if write_end == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned a fresh handle nobody else owns.
        Ok((read_end, unsafe { OwnedHandle::from_raw_handle(write_end) }))
    }

    /// The overlapped read end, with one event per kind of read: the drain
    /// thread's wait runs outside the state lock while reads that take
    /// bytes run under it, and a shared event could wake one for the
    /// other's completion.
    pub(super) struct Reader {
        pipe: OwnedHandle,
        /// Used by `read_some`, whose callers all hold the state lock.
        taken: OwnedHandle,
        /// Used by `wait_readable`, which only the drain thread calls.
        ready: OwnedHandle,
    }

    impl Reader {
        pub(super) fn new(pipe: OwnedHandle) -> io::Result<Self> {
            Ok(Self {
                pipe,
                taken: manual_reset_event()?,
                ready: manual_reset_event()?,
            })
        }
    }

    /// Bytes queued in the pipe right now.
    pub(super) fn pending(reader: &Reader) -> io::Result<usize> {
        let mut queued = 0u32;
        // SAFETY: no buffer is passed, and `queued` is a live u32 local.
        let peeked = unsafe {
            PeekNamedPipe(
                reader.pipe.as_raw_handle(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut queued,
                ptr::null_mut(),
            )
        };
        if peeked == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(queued as usize)
    }

    /// Reads no more than the pipe holds, so the read completes at once
    /// rather than waiting for a writer.
    pub(super) fn read_some(reader: &Reader, buf: &mut [u8]) -> io::Result<Got> {
        let queued = match pending(reader) {
            Ok(0) => return Ok(Got::Empty),
            Ok(queued) => queued,
            Err(err) if is_broken_pipe(&err) => return Ok(Got::Eof),
            Err(err) => return Err(err),
        };
        let len = queued.min(buf.len());
        match read_overlapped(&reader.pipe, &reader.taken, &mut buf[..len]) {
            Ok(n) => Ok(Got::Data(n)),
            Err(err) if is_broken_pipe(&err) => Ok(Got::Eof),
            Err(err) => Err(err),
        }
    }

    /// Blocks until the pipe holds bytes, by way of a zero-byte read that
    /// takes none of them. It fails once the pipe is empty and every
    /// writer has closed it.
    pub(super) fn wait_readable(reader: &Reader) -> io::Result<()> {
        read_overlapped(&reader.pipe, &reader.ready, &mut []).map(drop)
    }

    /// Waits for the read to finish before returning, so `buf` and the
    /// OVERLAPPED never go away while the kernel still holds them and no
    /// read is ever left to cancel.
    fn read_overlapped(
        pipe: &OwnedHandle,
        event: &OwnedHandle,
        buf: &mut [u8],
    ) -> io::Result<usize> {
        let mut overlapped = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..Default::default()
        };
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: `buf` holds at least `len` bytes, and it and `overlapped`
        // stay alive until GetOverlappedResult below has seen the read end.
        let started = unsafe {
            ReadFile(
                pipe.as_raw_handle(),
                buf.as_mut_ptr(),
                len,
                ptr::null_mut(),
                &mut overlapped,
            )
        };
        if started == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(err);
            }
        }
        let mut transferred = 0u32;
        // SAFETY: `overlapped` is the one the read started with, and the
        // wait flag makes the call return only once that read is done.
        if unsafe { GetOverlappedResult(pipe.as_raw_handle(), &overlapped, &mut transferred, 1) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred as usize)
    }

    fn is_broken_pipe(err: &io::Error) -> bool {
        err.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32)
    }

    fn manual_reset_event() -> io::Result<OwnedHandle> {
        // SAFETY: null attributes and name ask for an unnamed,
        // non-inheritable event; the result is checked.
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateEventW returned a fresh handle nobody else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(event) })
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
    use std::process::{Command, Stdio};
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

    /// Hooks that hold the drain thread before its first read until the
    /// returned sender fires, and report on the receiver once it exits.
    fn gated_hooks() -> (DrainHooks, mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (release, gate) = mpsc::channel();
        let (exited_tx, exited) = mpsc::channel();
        let hooks = DrainHooks {
            gate: Some(gate),
            exited: Some(exited_tx),
        };
        (hooks, release, exited)
    }

    struct KillOnDrop(libc::pid_t);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: kill takes no pointers.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
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

    /// A child that never stops writing cannot keep `finish` reading:
    /// it takes what was queued when it began and returns within the
    /// bound.
    #[test]
    #[serial]
    fn finish_never_waits_for_a_streaming_writer() {
        let (saved, reader) = capture_stdout();
        let guard = FdRedirect::begin(|_| {}).unwrap();
        let spawned = Command::new("sh")
            .arg("-c")
            .arg("(while :; do echo x; done) >&2 & echo $!")
            .stderr(Stdio::inherit())
            .output()
            .unwrap();
        let pid = String::from_utf8_lossy(&spawned.stdout)
            .trim()
            .parse()
            .unwrap();
        let _writer = KillOnDrop(pid);
        let started = Instant::now();
        guard.finish();
        let elapsed = started.elapsed();
        release_harness_output(saved, reader, "");
        assert!(elapsed < Duration::from_secs(2), "finish took {elapsed:?}");
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

    /// Output written before `finish` lands in the cell even when the
    /// drain thread has not run at all: `finish` returns while the thread
    /// is held at its gate, and once it has exited nothing reached the
    /// late callback.
    #[test]
    #[serial]
    fn pre_finish_output_never_depends_on_the_drain_thread() {
        let (saved, reader) = capture_stdout();
        let (tx, rx) = mpsc::channel();
        let (hooks, release, exited) = gated_hooks();
        let guard = FdRedirect::begin_with(
            move |text| {
                let _ = tx.send(text);
            },
            hooks,
        )
        .unwrap();
        write_fd1(b"slow-drain-r6x\n");
        let captured = guard.finish();
        release.send(()).unwrap();
        exited.recv_timeout(Duration::from_secs(5)).unwrap();
        release_harness_output(saved, reader, "");
        assert_eq!(captured.trim(), "slow-drain-r6x");
        assert!(rx.try_recv().is_err(), "the output went out as late output");
    }

    /// An unfinished guard dropped before the drain thread has run hands
    /// what the pipe holds to the late callback, exactly once.
    #[test]
    #[serial]
    fn dropping_a_guard_before_the_drain_runs_forwards_its_output_once() {
        let (saved, reader) = capture_stdout();
        let (tx, rx) = mpsc::channel();
        let (hooks, release, exited) = gated_hooks();
        let guard = FdRedirect::begin_with(
            move |text| {
                let _ = tx.send(text);
            },
            hooks,
        )
        .unwrap();
        write_fd1(b"gated-drop-r7x\n");
        drop(guard);
        release.send(()).unwrap();
        exited.recv_timeout(Duration::from_secs(5)).unwrap();
        release_harness_output(saved, reader, "");
        let late: Vec<String> = rx.try_iter().collect();
        assert_eq!(late, ["gated-drop-r7x\n"]);
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

/// Every test here holds the `io::stdout()` lock for as long as its guard
/// lives: the harness reports from other threads through `io::stdout()`,
/// which follows the swapped std handle, so without the lock those lines
/// would land in the capture pipe and vanish with it.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use serial_test::serial;
    use std::fs::File;
    use std::os::windows::io::AsHandle;
    use std::process::Command;
    use std::sync::mpsc;

    fn write_std_streams(out: &mut io::StdoutLock<'_>, marker: &str) {
        out.write_all(format!("stdout-{marker}\n").as_bytes())
            .unwrap();
        out.flush().unwrap();
        io::stderr()
            .write_all(format!("stderr-{marker}\n").as_bytes())
            .unwrap();
        io::stderr().flush().unwrap();
    }

    /// Hooks that hold the drain thread before its first read until the
    /// returned sender fires, and report on the receiver once it exits.
    fn gated_hooks() -> (DrainHooks, mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (release, gate) = mpsc::channel();
        let (exited_tx, exited) = mpsc::channel();
        let hooks = DrainHooks {
            gate: Some(gate),
            exited: Some(exited_tx),
        };
        (hooks, release, exited)
    }

    /// Rust's std streams look the std handles up on every write, so
    /// both markers land in the cell. The test writes through the stream
    /// handles directly because the harness captures the macros first.
    #[test]
    #[serial]
    fn region_output_lands_in_the_cell() {
        let mut out = io::stdout().lock();
        let guard = FdRedirect::begin(|_| {}).unwrap();
        write_std_streams(&mut out, "marker-w1x");
        let captured = guard.finish();
        assert!(captured.contains("stdout-marker-w1x"), "{captured}");
        assert!(captured.contains("stderr-marker-w1x"), "{captured}");
    }

    /// The panic hook's view of the capture follows the guard exactly,
    /// whether it ends through `finish` or a drop.
    #[test]
    #[serial]
    fn the_capture_flag_tracks_the_guard() {
        let _out = io::stdout().lock();
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
        let mut out = io::stdout().lock();
        let (tx, rx) = mpsc::channel();
        let guard = FdRedirect::begin(move |text| {
            let _ = tx.send(text);
        })
        .unwrap();
        write_std_streams(&mut out, "buffered-w5x");
        // Give the drain thread time to move the bytes into the buffer,
        // so it is the drop that forwards them.
        std::thread::sleep(Duration::from_millis(100));
        drop(guard);
        let late = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(late.contains("stdout-buffered-w5x"), "{late}");
    }

    /// Output written before `finish` lands in the cell even when the
    /// drain thread has not run at all: `finish` returns while the thread
    /// is held at its gate, and once it has exited nothing reached the
    /// late callback.
    #[test]
    #[serial]
    fn pre_finish_output_never_depends_on_the_drain_thread() {
        let mut out = io::stdout().lock();
        let (tx, rx) = mpsc::channel();
        let (hooks, release, exited) = gated_hooks();
        let guard = FdRedirect::begin_with(
            move |text| {
                let _ = tx.send(text);
            },
            hooks,
        )
        .unwrap();
        write_std_streams(&mut out, "slow-drain-w6x");
        let captured = guard.finish();
        release.send(()).unwrap();
        exited.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(captured, "stdout-slow-drain-w6x\nstderr-slow-drain-w6x\n");
        assert!(rx.try_recv().is_err(), "the output went out as late output");
    }

    /// An unfinished guard dropped before the drain thread has run hands
    /// what the pipe holds to the late callback, exactly once.
    #[test]
    #[serial]
    fn dropping_a_guard_before_the_drain_runs_forwards_its_output_once() {
        let mut out = io::stdout().lock();
        let (tx, rx) = mpsc::channel();
        let (hooks, release, exited) = gated_hooks();
        let guard = FdRedirect::begin_with(
            move |text| {
                let _ = tx.send(text);
            },
            hooks,
        )
        .unwrap();
        write_std_streams(&mut out, "gated-drop-w7x");
        drop(guard);
        release.send(()).unwrap();
        exited.recv_timeout(Duration::from_secs(5)).unwrap();
        let late: Vec<String> = rx.try_iter().collect();
        assert_eq!(late, ["stdout-gated-drop-w7x\nstderr-gated-drop-w7x\n"]);
    }

    /// A writer still holding the pipe (here a duplicate of the write end,
    /// as a child would hold one) does not hold up `finish`, and what it
    /// writes afterwards reaches the late callback.
    #[test]
    #[serial]
    fn a_lingering_writer_neither_delays_finish_nor_loses_late_output() {
        let mut out = io::stdout().lock();
        let (tx, rx) = mpsc::channel();
        let guard = FdRedirect::begin(move |text| {
            let _ = tx.send(text);
        })
        .unwrap();
        let lingering = File::from(out.as_handle().try_clone_to_owned().unwrap());
        write_std_streams(&mut out, "before-w2x");
        let started = Instant::now();
        let captured = guard.finish();
        let elapsed = started.elapsed();
        (&lingering).write_all(b"late-w3x\n").unwrap();
        let late = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(lingering);
        assert!(elapsed < Duration::from_secs(2), "finish took {elapsed:?}");
        assert_eq!(captured, "stdout-before-w2x\nstderr-before-w2x\n");
        assert_eq!(late, "late-w3x\n");
    }

    /// A child spawned with inherited stdio gets the swapped std handles,
    /// so what it prints is part of the cell.
    #[test]
    #[serial]
    fn a_child_with_inherited_stdio_writes_into_the_cell() {
        let _out = io::stdout().lock();
        let guard = FdRedirect::begin(|_| {}).unwrap();
        Command::new("cmd")
            .args(["/c", "echo child-marker-x7x"])
            .status()
            .unwrap();
        let captured = guard.finish();
        assert!(captured.contains("child-marker-x7x"), "{captured}");
    }

    /// The process's std handles as `GetStdHandle` reports them.
    fn std_handles() -> (isize, isize) {
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        };
        // SAFETY: GetStdHandle takes no pointers and only reads process state.
        unsafe {
            (
                GetStdHandle(STD_OUTPUT_HANDLE) as isize,
                GetStdHandle(STD_ERROR_HANDLE) as isize,
            )
        }
    }

    /// Whichever way the guard ends, the std handles are the ones the
    /// process had before `begin`; while it lives they are the pipe.
    #[test]
    #[serial]
    fn the_std_handles_are_put_back_after_finish_and_after_drop() {
        let _out = io::stdout().lock();
        let before = std_handles();

        let guard = FdRedirect::begin(|_| {}).unwrap();
        assert_ne!(std_handles(), before);
        guard.finish();
        assert_eq!(std_handles(), before);

        let guard = FdRedirect::begin(|_| {}).unwrap();
        assert_ne!(std_handles(), before);
        drop(guard);
        assert_eq!(std_handles(), before);
    }
}
