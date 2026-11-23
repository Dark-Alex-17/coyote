use anyhow::Result;
use serde_json::{Value, json};
#[cfg(not(any(unix, windows)))]
compile_error!("the prompt reply channel has a unix half and a windows half only");

#[cfg(windows)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::io::{self, PipeWriter, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::process::Command;
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::sync::{Condvar, Mutex};

/// The parent's end of the reply pipe. The child reads replies from the fd
/// named by `env_value`, which `configure` keeps open across exec.
#[cfg(unix)]
pub(crate) struct Server {
    pub(super) writer: PipeWriter,
    pub(super) child_end: Option<OwnedFd>,
    pub(super) child_fd: RawFd,
}

#[cfg(unix)]
impl Server {
    /// The child's read end sits at fd 10 or above: `Stdio` redirects 0-2
    /// in the child and the bash helper saves its stdout on fd 9, so a
    /// lower number would be clobbered before the tool reads from it.
    pub(crate) fn open() -> Result<Self> {
        let (reader, writer) = io::pipe()?;
        // SAFETY: fcntl with F_DUPFD_CLOEXEC only reads the descriptor and
        // returns a fresh one; `reader` stays valid for the call.
        let child_fd = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        if child_fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: `child_fd` was just returned by fcntl, is open, and nothing
        // else owns it.
        let child_end = unsafe { OwnedFd::from_raw_fd(child_fd) };
        drop(reader);
        Ok(Self {
            writer,
            child_end: Some(child_end),
            child_fd,
        })
    }

    pub(crate) fn env_value(&self) -> String {
        format!("/dev/fd/{}", self.child_fd)
    }

    /// Both pipe ends are CLOEXEC; the child needs its read end to survive
    /// exec, and only that one.
    pub(crate) fn configure(&self, cmd: &mut Command) {
        let fd = self.child_fd;
        // SAFETY: the closure runs between fork and exec and only calls
        // fcntl, which is async-signal-safe; it touches no shared state.
        unsafe {
            cmd.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Drops the parent's copy of the child's read end, so a write after the
    /// tool exits fails with a broken pipe instead of filling a dead pipe.
    pub(crate) fn spawned(&mut self) {
        self.child_end.take();
    }

    pub(crate) fn reply(&self, id: &str, answer: &Value) {
        let mut line = json!({"id": id, "answer": answer}).to_string();
        line.push('\n');
        let mut writer = &self.writer;
        match writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.flush())
        {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                debug!("Prompt reply for '{id}' dropped: tool already exited")
            }
            Err(err) => warn!("Unable to write the prompt reply for '{id}': {err}"),
        }
    }
}

/// The parent's end of the reply pipe: a named pipe the child opens by the
/// path in `env_value`. The helpers open that path afresh per prompt, and
/// bash's `read <"$path"` even per line (Git Bash may open it once more to
/// probe it first), so the pipe takes one client after another. Acceptor
/// threads each keep an instance listening and queue every client that
/// connects; a delivery thread hands the queued lines to those clients in
/// order. A line leaves the queue only once a client has read it, so a
/// client that closes early leaves it for the next one, the way an unread
/// line stays in a unix pipe.
#[cfg(windows)]
pub(crate) struct Server {
    name: Vec<u16>,
    path: String,
    shared: Arc<Shared>,
}

/// How many instances listen at once. With one, a client that connects
/// and leaves at once (Git Bash probing the path) takes the only instance,
/// and an open that follows before its replacement exists is refused as
/// busy; with two, one always listens while the other is replaced.
#[cfg(windows)]
const ACCEPTORS: usize = 2;

#[cfg(windows)]
struct Shared {
    delivery: Mutex<Delivery>,
    wake: Condvar,
}

#[cfg(windows)]
impl Shared {
    fn new() -> Self {
        Self {
            delivery: Mutex::new(Delivery {
                lines: VecDeque::new(),
                clients: VecDeque::new(),
                acceptors: 0,
                closed: false,
                #[cfg(test)]
                accepted: 0,
            }),
            wake: Condvar::new(),
        }
    }
}

#[cfg(windows)]
struct Delivery {
    lines: VecDeque<String>,
    /// Connected clients not served yet, oldest first.
    clients: VecDeque<OwnedHandle>,
    /// Acceptor threads still able to keep an instance listening.
    acceptors: usize,
    closed: bool,
    /// Clients accepted so far, each counted once the instance replacing
    /// the one it took is listening.
    #[cfg(test)]
    accepted: usize,
}

#[cfg(windows)]
impl Server {
    /// The name carries a random suffix: the pipe namespace is shared by
    /// every process on the machine, and the first instance claims the
    /// name, so a guessable one could be taken before the server gets to it.
    pub(crate) fn open() -> Result<Self> {
        let path = format!(
            r"\\.\pipe\coyote-prompt-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let name = win::wide(&path);
        let descriptor = Arc::new(win::OwnerOnlyDescriptor::new()?);
        let first = win::create_instance(&name, true, &descriptor)?;
        let shared = Arc::new(Shared::new());
        start_threads(&name, descriptor, Some(first), &shared)?;
        Ok(Self { name, path, shared })
    }

    pub(crate) fn env_value(&self) -> String {
        self.path.clone()
    }

    /// The child reaches the pipe by name; no handle crosses the spawn.
    pub(crate) fn configure(&self, _cmd: &mut Command) {}

    pub(crate) fn spawned(&mut self) {}

    pub(crate) fn reply(&self, id: &str, answer: &Value) {
        let mut line = json!({"id": id, "answer": answer}).to_string();
        line.push('\n');
        let mut delivery = self.shared.delivery.lock().unwrap();
        if delivery.closed {
            warn!("Unable to write the prompt reply for '{id}': the reply pipe is gone");
            return;
        }
        delivery.lines.push_back(line);
        drop(delivery);
        self.shared.wake.notify_all();
    }
}

/// Ends the threads without joining them: a client that connected but
/// never reads would otherwise hold the drop hostage. Lines still queued
/// are lost; the tool they were for is gone. A stalled client holds the
/// delivery thread only for the tool's lifetime: the tool's death closes
/// the client handle, which fails the flush the thread is parked in. Each
/// acceptor is parked on its own listening instance, so each gets a wake.
#[cfg(windows)]
impl Drop for Server {
    fn drop(&mut self) {
        let mut delivery = self.shared.delivery.lock().unwrap();
        delivery.closed = true;
        if !delivery.lines.is_empty() {
            debug!(
                "{} prompt reply line(s) dropped: tool already exited",
                delivery.lines.len()
            );
        }
        drop(delivery);
        self.shared.wake.notify_all();
        for _ in 0..ACCEPTORS {
            win::wake(&self.name);
        }
    }
}

/// Starts the delivery thread and the acceptors. The first acceptor takes
/// `listener`, or claims the name afresh when there is none; the others
/// start only once it holds an instance, so the name is always claimed by
/// a first instance rather than joined.
#[cfg(windows)]
fn start_threads(
    name: &[u16],
    descriptor: Arc<win::OwnerOnlyDescriptor>,
    listener: Option<OwnedHandle>,
    shared: &Arc<Shared>,
) -> std::io::Result<()> {
    shared.delivery.lock().unwrap().acceptors = 1;
    let delivery_shared = Arc::clone(shared);
    std::thread::Builder::new()
        .name("prompt-reply".into())
        .spawn(move || deliver_lines(&delivery_shared))?;
    let name = name.to_vec();
    let shared = Arc::clone(shared);
    std::thread::Builder::new()
        .name("prompt-accept".into())
        .spawn(move || {
            let first = match listener {
                Some(pipe) => pipe,
                None => match win::reclaim_instance(&name, &descriptor) {
                    Ok(pipe) => pipe,
                    Err(err) => return acceptor_gone(&name, &shared, &err),
                },
            };
            for _ in 1..ACCEPTORS {
                shared.delivery.lock().unwrap().acceptors += 1;
                let (thread_name, thread_descriptor, thread_shared) =
                    (name.clone(), Arc::clone(&descriptor), Arc::clone(&shared));
                let spawned = std::thread::Builder::new()
                    .name("prompt-accept".into())
                    .spawn(move || {
                        accept_clients(&thread_name, &thread_descriptor, None, &thread_shared)
                    });
                if let Err(err) = spawned {
                    acceptor_gone(&name, &shared, &err);
                }
            }
            accept_clients(&name, &descriptor, Some(first), &shared);
        })?;
    Ok(())
}

/// One acceptor: keep an instance listening, and when a client connects,
/// put a replacement up before queueing the client, so an open that comes
/// right after finds one listening. `listener` is the instance to start
/// with; without one a new instance joins the pipe the other acceptors
/// already hold.
#[cfg(windows)]
fn accept_clients(
    name: &[u16],
    descriptor: &win::OwnerOnlyDescriptor,
    listener: Option<OwnedHandle>,
    shared: &Shared,
) {
    let mut listener = listener;
    loop {
        let pipe = match listener.take() {
            Some(pipe) => pipe,
            None => match next_instance(name, descriptor) {
                Ok(pipe) => pipe,
                Err(err) => return acceptor_gone(name, shared, &err),
            },
        };
        // A wake issued before this instance existed found nothing to
        // connect to, so the flag has to be re-read now that it does.
        if shared.delivery.lock().unwrap().closed {
            return;
        }
        let arrival = match win::connect(&pipe) {
            Ok(arrival) => arrival,
            Err(err) => {
                debug!("Prompt reply pipe instance failed while listening: {err}");
                continue;
            }
        };
        if shared.delivery.lock().unwrap().closed {
            return;
        }
        match next_instance(name, descriptor) {
            Ok(next) => listener = Some(next),
            Err(err) => debug!("Unable to create the next prompt reply pipe instance: {err}"),
        }
        let mut delivery = shared.delivery.lock().unwrap();
        if delivery.closed {
            return;
        }
        match arrival {
            win::Arrival::Present => delivery.clients.push_back(pipe),
            // Opened and closed before the accept (Git Bash probing the
            // path): nothing to serve, and the instance must be
            // disconnected before it is released.
            win::Arrival::Gone => win::disconnect(&pipe),
        }
        #[cfg(test)]
        {
            delivery.accepted += 1;
        }
        drop(delivery);
        shared.wake.notify_all();
    }
}

/// A further instance of the pipe the acceptors hold. When joining it
/// fails, the name is claimed afresh as a first instance instead; a client
/// still holding a previous instance keeps the name alive, which only
/// defers that claim until it lets go.
#[cfg(windows)]
fn next_instance(
    name: &[u16],
    descriptor: &win::OwnerOnlyDescriptor,
) -> std::io::Result<OwnedHandle> {
    win::create_instance(name, false, descriptor)
        .or_else(|_| win::reclaim_instance(name, descriptor))
}

/// An acceptor that cannot keep an instance up stops; once none is left
/// the channel is closed, so `reply` warns instead of queueing lines no
/// client can reach.
#[cfg(windows)]
fn acceptor_gone(name: &[u16], shared: &Shared, err: &std::io::Error) {
    let mut delivery = shared.delivery.lock().unwrap();
    delivery.acceptors = delivery.acceptors.saturating_sub(1);
    // A server that is already closing has nothing to deliver, so a failed
    // instance on its way out is noise.
    if delivery.closed {
        return;
    }
    warn!(
        "Unable to recreate the prompt reply pipe {}: {err}",
        String::from_utf16_lossy(&name[..name.len().saturating_sub(1)])
    );
    if delivery.acceptors == 0 {
        delivery.closed = true;
        drop(delivery);
        shared.wake.notify_all();
    }
}

/// The delivery thread: serve the oldest queued client until it goes
/// away, then the next one.
#[cfg(windows)]
fn deliver_lines(shared: &Shared) {
    loop {
        let pipe = {
            let mut delivery = shared.delivery.lock().unwrap();
            loop {
                if delivery.closed {
                    return;
                }
                if let Some(pipe) = delivery.clients.pop_front() {
                    break pipe;
                }
                delivery = shared.wake.wait(delivery).unwrap();
            }
        };
        let closed = serve(&pipe, shared);
        win::disconnect(&pipe);
        if closed {
            return;
        }
    }
}

/// Hands queued lines to the connected client. Returns `true` when the
/// server closed, `false` when the client went away; in the latter case the
/// line it did not take is still at the front of the queue.
#[cfg(windows)]
fn serve(pipe: &OwnedHandle, shared: &Shared) -> bool {
    loop {
        let line = {
            let mut delivery = shared.delivery.lock().unwrap();
            loop {
                if delivery.closed {
                    return true;
                }
                if let Some(line) = delivery.lines.front() {
                    break line.clone();
                }
                delivery = shared.wake.wait(delivery).unwrap();
            }
        };
        match win::deliver(pipe, line.as_bytes()) {
            Ok(()) => {
                shared.delivery.lock().unwrap().lines.pop_front();
            }
            Err(err) => {
                debug!("Prompt reply client closed before the next line: {err}");
                return false;
            }
        }
    }
}

/// Owned wrappers around the Win32 named-pipe calls the server makes.
#[cfg(windows)]
mod win {
    use std::ffi::{OsStr, c_void};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, GENERIC_READ,
        INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, FlushFileBuffers,
        OPEN_EXISTING, PIPE_ACCESS_OUTBOUND, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        WriteFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const OUT_BUFFER: u32 = 64 * 1024;

    pub(super) fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// A security descriptor granting the current user full access and
    /// nobody else anything: the pipe namespace is visible to every account
    /// on the machine, and the replies are not for the other ones. The
    /// mandatory label keeps a low-integrity process of the same user
    /// (a sandboxed browser, say) from reading them, and from writing to
    /// the pipe or creating an instance under its name to forge one.
    pub(super) struct OwnerOnlyDescriptor(*mut c_void);

    // SAFETY: the descriptor is an opaque heap buffer that is only read
    // after construction; nothing ties it to the thread that built it.
    unsafe impl Send for OwnerOnlyDescriptor {}
    // SAFETY: after construction the buffer is only read, by the pipe
    // calls the acceptors make, and concurrent reads of it are sound.
    unsafe impl Sync for OwnerOnlyDescriptor {}

    impl OwnerOnlyDescriptor {
        pub(super) fn new() -> io::Result<Self> {
            let sid = current_user_sid()?;
            let sddl = wide(&format!("D:P(A;;GA;;;{sid})S:(ML;;NRNW;;;ME)"));
            let mut descriptor = ptr::null_mut();
            // SAFETY: `sddl` is NUL terminated and outlives the call; the
            // out pointer is a valid local and the size out pointer may be
            // null.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        pub(super) fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for OwnerOnlyDescriptor {
        fn drop(&mut self) {
            // SAFETY: the buffer came from the SDDL conversion, which
            // documents LocalFree as its release, and is freed once.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn current_user_sid() -> io::Result<String> {
        let mut token = ptr::null_mut();
        // SAFETY: the process pseudo handle needs no release; the out
        // pointer is a valid local.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OpenProcessToken succeeded and this function alone owns
        // the handle it returned.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut needed = 0u32;
        // SAFETY: a zero-length query only reports the size needed through
        // the valid local out pointer.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64 cells keep the TOKEN_USER pointer field aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        // SAFETY: the buffer holds at least `needed` bytes, the size the
        // previous call reported for this token.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call filled the buffer with a TOKEN_USER whose SID
        // pointer targets the same buffer, which is alive for the read.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text = ptr::null_mut();
        // SAFETY: `sid` is a valid SID inside `buffer`; the out pointer is a
        // valid local.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the conversion returned a NUL-terminated wide string that
        // stays valid until the LocalFree below.
        let string = unsafe {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let string = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            string
        };
        Ok(string)
    }

    /// One server end of the pipe `name`, waiting for a client. The first
    /// instance claims the name, so a pipe some other process already
    /// created under it is an error rather than a shared channel.
    pub(super) fn create_instance(
        name: &[u16],
        first: bool,
        descriptor: &OwnerOnlyDescriptor,
    ) -> io::Result<OwnedHandle> {
        let mut open_mode = PIPE_ACCESS_OUTBOUND;
        if first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let attributes = descriptor.attributes();
        // SAFETY: `name` is NUL terminated and `attributes` points at a
        // live descriptor; both outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                OUT_BUFFER,
                0,
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateNamedPipeW returned a fresh handle nobody else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    /// The first instance of `name` once no server instance of ours is
    /// left. The name stays alive while a client handle to the previous
    /// instance is open, and claiming it meanwhile is denied, so the
    /// claim is retried for about a second: a helper closes its handle
    /// as soon as its `read` returns.
    pub(super) fn reclaim_instance(
        name: &[u16],
        descriptor: &OwnerOnlyDescriptor,
    ) -> io::Result<OwnedHandle> {
        const ATTEMPTS: u32 = 50;
        const PAUSE: Duration = Duration::from_millis(20);
        let mut attempt = 0;
        loop {
            match create_instance(name, true, descriptor) {
                Err(err)
                    if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
                        && attempt < ATTEMPTS =>
                {
                    attempt += 1;
                    std::thread::sleep(PAUSE);
                }
                result => return result,
            }
        }
    }

    /// A client that reached a listening instance.
    pub(super) enum Arrival {
        /// Connected and still holding its end.
        Present,
        /// Opened the instance and closed it again before the accept.
        Gone,
    }

    /// Blocks until a client opens `pipe`. A client that opened it between
    /// creation and this call counts as connected (`ERROR_PIPE_CONNECTED`),
    /// and one that also closed it already arrived all the same
    /// (`ERROR_NO_DATA`).
    pub(super) fn connect(pipe: &OwnedHandle) -> io::Result<Arrival> {
        // SAFETY: `pipe` is a live named-pipe server handle; a null
        // OVERLAPPED selects the blocking form.
        if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), ptr::null_mut()) } != 0 {
            return Ok(Arrival::Present);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == ERROR_PIPE_CONNECTED as i32 => Ok(Arrival::Present),
            Some(code) if code == ERROR_NO_DATA as i32 => Ok(Arrival::Gone),
            _ => Err(err),
        }
    }

    /// Writes `line` and returns once the client has read every byte of
    /// it, so the line is known to have arrived before it leaves the
    /// queue. The helpers read a byte at a time up to the newline, so the
    /// flush does not wait on bytes the client will never ask for. A client
    /// that closes before reading makes the write or the flush fail, which
    /// is what keeps the line at the front of the queue for the next one;
    /// `each_reopen_of_the_pipe_receives_the_next_queued_line` pins it.
    pub(super) fn deliver(pipe: &OwnedHandle, line: &[u8]) -> io::Result<()> {
        let mut rest = line;
        while !rest.is_empty() {
            let mut written = 0u32;
            let len = u32::try_from(rest.len()).unwrap_or(u32::MAX);
            // SAFETY: `rest` is valid for `len` bytes; the count out pointer
            // is a valid local; a null OVERLAPPED selects the blocking form.
            let ok = unsafe {
                WriteFile(
                    pipe.as_raw_handle(),
                    rest.as_ptr(),
                    len,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            rest = &rest[written as usize..];
        }
        // SAFETY: `pipe` is a live named-pipe server handle.
        if unsafe { FlushFileBuffers(pipe.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn disconnect(pipe: &OwnedHandle) {
        // SAFETY: `pipe` is a live named-pipe server handle.
        unsafe {
            DisconnectNamedPipe(pipe.as_raw_handle());
        }
    }

    /// Opens the pipe as a client once, so a thread blocked in
    /// `ConnectNamedPipe` returns. Failure means nothing was listening,
    /// in which case the thread is not blocked there. Identification-level
    /// impersonation is the most a server at this name may do with the
    /// caller's token, in case something else claimed the name first.
    pub(super) fn wake(name: &[u16]) {
        // SAFETY: `name` is NUL terminated; the null attributes and
        // template are the documented defaults.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_NONE,
                ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: the handle was just returned by CreateFileW and is
            // closed exactly once.
            unsafe {
                CloseHandle(handle);
            }
        }
    }
}

/// What a tool sees of the reply channel, the same on both platforms: the
/// server is driven only through the calls the tool runner makes, and the
/// client reaches the channel the way a spawned tool does, through the
/// value of `COYOTE_PROMPT_REPLY`. Every read that could block runs on a
/// helper thread and is collected with a deadline, so a regression fails
/// the test instead of hanging it.
#[cfg(test)]
mod contract;

/// The named-pipe server stands in for the unix fd pipe; these pin the
/// shape the helpers depend on beyond the shared contract. Every read
/// that could block runs on a helper thread and is collected with a
/// generous timeout, so a regression fails the test instead of hanging it.
#[cfg(all(test, windows))]
mod windows_tests;
