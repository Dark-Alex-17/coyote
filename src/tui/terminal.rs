//! Terminal lifecycle for the full-screen UI. Every escape sequence goes
//! through a descriptor dup'd from stdout once at start, so a command turn
//! that redirects fd 1 into its capture pipe never swallows a draw or the
//! restore, and the panic hook writes the leave sequence through that same
//! descriptor before the default hook prints.

use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::{Buffer, Cell, CellDiffOption, CellWidth};
use std::io::{self, Write};
use std::sync::Once;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
pub(crate) type TtyWriter = std::fs::File;
#[cfg(not(unix))]
pub(crate) type TtyWriter = io::Stdout;

/// Raw fd of the dup'd stdout the panic hook restores through; -1 until
/// `Screen::enter` has run, and again once the `Screen` is gone.
#[cfg(unix)]
static PANIC_FD: AtomicI32 = AtomicI32::new(-1);
/// Whether the keyboard enhancement flags are currently pushed, so the
/// leave path pops them exactly when the enter path pushed them.
static ENHANCEMENT_PUSHED: AtomicBool = AtomicBool::new(false);
/// Whether `enter` has run without a matching `leave`. Keyed here rather
/// than on crossterm's raw-mode flag because a command handler may toggle
/// raw mode itself mid-session, which must not make `leave` skip the
/// restore.
static SCREEN_ENTERED: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// One terminal mode toggled on enter and back off on leave. The `bool` is
/// the state the step moves the terminal to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalStep {
    RawMode(bool),
    AlternateScreen(bool),
    /// Ratatui hides the cursor on every frame and only shows it again
    /// from `Terminal`'s Drop, which a hard exit, a suspend and the panic
    /// hook never reach; the leave sequence shows it itself.
    CursorHidden(bool),
    BracketedPaste(bool),
    MouseCapture(bool),
    KeyboardEnhancement(bool),
}

impl TerminalStep {
    /// The step that undoes this one.
    pub(crate) fn inverse(self) -> Self {
        match self {
            Self::RawMode(on) => Self::RawMode(!on),
            Self::AlternateScreen(on) => Self::AlternateScreen(!on),
            Self::CursorHidden(on) => Self::CursorHidden(!on),
            Self::BracketedPaste(on) => Self::BracketedPaste(!on),
            Self::MouseCapture(on) => Self::MouseCapture(!on),
            Self::KeyboardEnhancement(on) => Self::KeyboardEnhancement(!on),
        }
    }

    fn apply(self, writer: &mut impl Write) -> io::Result<()> {
        match self {
            Self::RawMode(true) => enable_raw_mode(),
            Self::RawMode(false) => disable_raw_mode(),
            Self::AlternateScreen(true) => execute!(writer, EnterAlternateScreen),
            Self::AlternateScreen(false) => execute!(writer, LeaveAlternateScreen),
            Self::CursorHidden(true) => execute!(writer, Hide),
            Self::CursorHidden(false) => execute!(writer, Show),
            Self::BracketedPaste(true) => execute!(writer, EnableBracketedPaste),
            Self::BracketedPaste(false) => execute!(writer, DisableBracketedPaste),
            Self::MouseCapture(true) => execute!(writer, EnableMouseCapture),
            Self::MouseCapture(false) => execute!(writer, DisableMouseCapture),
            Self::KeyboardEnhancement(true) => execute!(
                writer,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            ),
            Self::KeyboardEnhancement(false) => execute!(writer, PopKeyboardEnhancementFlags),
        }
    }
}

/// The modes switched on when the UI takes the terminal, in order. The
/// enhancement flags are pushed last, after the alternate screen is up, and
/// only when the terminal answered the probe.
pub(crate) fn enter_sequence(enhancement: bool) -> Vec<TerminalStep> {
    let mut steps = vec![
        TerminalStep::RawMode(true),
        TerminalStep::AlternateScreen(true),
        TerminalStep::CursorHidden(true),
        TerminalStep::BracketedPaste(true),
        TerminalStep::MouseCapture(true),
    ];
    if enhancement {
        steps.push(TerminalStep::KeyboardEnhancement(true));
    }
    steps
}

/// The strict mirror of [`enter_sequence`]: the same modes switched off in
/// reverse order.
pub(crate) fn leave_sequence(enhancement: bool) -> Vec<TerminalStep> {
    enter_sequence(enhancement)
        .into_iter()
        .rev()
        .map(TerminalStep::inverse)
        .collect()
}

/// Asks the terminal whether it speaks the kitty keyboard protocol. Reads
/// the tty, so it must run before the event stream owns stdin.
pub(crate) fn probe_keyboard_enhancement() -> bool {
    supports_keyboard_enhancement().unwrap_or(false)
}

/// A second handle on stdout, independent of what a later `dup2` puts on
/// fd 1.
#[cfg(unix)]
pub(crate) fn open_tty_writer() -> io::Result<TtyWriter> {
    use std::os::fd::FromRawFd;
    // SAFETY: fcntl with F_DUPFD_CLOEXEC takes no pointers; the result is checked.
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just created by fcntl and nothing else owns it.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
pub(crate) fn open_tty_writer() -> io::Result<TtyWriter> {
    Ok(io::stdout())
}

/// The tty's current line-discipline settings.
#[cfg(unix)]
fn tty_modes(fd: i32) -> io::Result<libc::termios> {
    let mut modes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr fills one termios through a valid pointer; the
    // result is checked before the value is read.
    if unsafe { libc::tcgetattr(fd, modes.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a zero return means tcgetattr initialised `modes`.
    Ok(unsafe { modes.assume_init() })
}

#[cfg(unix)]
fn set_tty_modes(fd: i32, modes: &libc::termios) -> io::Result<()> {
    // SAFETY: tcsetattr reads one termios through a valid pointer; the
    // result is checked.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, modes) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Installs the restoring panic hook in front of whatever hook is current.
/// Returns whether this call installed it; later calls are no-ops.
pub(crate) fn install_panic_hook() -> bool {
    let mut installed = false;
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_from_static();
            echo_panic_past_capture(info);
            previous(info);
        }));
        installed = true;
    });
    installed
}

/// The terminal behind the descriptor `enter` recorded, or None before
/// `Screen::enter` and after the `Screen` is gone.
#[cfg(unix)]
fn static_tty() -> Option<std::mem::ManuallyDrop<std::fs::File>> {
    use std::os::fd::FromRawFd;
    // Read, not taken: the fd stays armed for the next panic, since a
    // background task's panic is caught as a join error and the UI
    // re-enters the screen afterwards. `leave` itself is single-taker
    // through SCREEN_ENTERED, so two hooks cannot both write.
    let fd = PANIC_FD.load(Ordering::SeqCst);
    (fd >= 0).then(|| {
        // SAFETY: the fd was dup'd by `open_tty_writer`; `Screen` clears
        // PANIC_FD before the writer that owns the fd drops, so this write
        // normally lands on an open descriptor. A hook racing `Screen::drop`
        // on another thread can still find it closed and get EBADF, which
        // is ignored like every other error here. ManuallyDrop keeps this
        // handle from closing the fd.
        std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) })
    })
}

/// Writes the leave sequence through the descriptor `enter` recorded. Runs
/// on the panic path, so every error is ignored: there is nothing left to
/// report it to.
fn restore_from_static() {
    #[cfg(unix)]
    if let Some(mut writer) = static_tty() {
        let _ = leave(&mut *writer);
    }
    #[cfg(not(unix))]
    {
        let _ = leave(&mut io::stdout());
    }
}

/// The hook after this one prints to fd 2, which during a command turn is
/// the capture pipe. A panic on a work thread is caught as a join error
/// and its text comes back through the late-output path; the driver's own
/// panic ends the process with the text still in the pipe, so it is
/// written to the terminal here as well, after `leave` has cooked it.
#[cfg(unix)]
fn echo_panic_past_capture(info: &std::panic::PanicHookInfo<'_>) {
    if super::redirect::fds_captured()
        && std::thread::current().name() == Some(crate::MAIN_THREAD_NAME)
        && let Some(mut tty) = static_tty()
    {
        // The payload is whatever the panicking code formatted; it goes
        // to the restored terminal stripped of anything it could do there.
        let message = super::render::plain_text(&info.to_string());
        let _ = writeln!(tty, "thread '{}' {message}", crate::MAIN_THREAD_NAME);
    }
}

#[cfg(not(unix))]
fn echo_panic_past_capture(_: &std::panic::PanicHookInfo<'_>) {}

/// Switches the terminal into the UI's modes.
pub(crate) fn enter(writer: &mut impl Write, enhancement: bool) -> io::Result<()> {
    install_panic_hook();
    enter_steps(writer, enter_sequence(enhancement))
}

/// Applies `steps` in order. A failing step hands the terminal back through
/// `leave`, which undoes the full sequence (switching off a mode that never
/// came on is harmless) and clears the entered flag.
fn enter_steps(writer: &mut impl Write, steps: Vec<TerminalStep>) -> io::Result<()> {
    SCREEN_ENTERED.store(true, Ordering::SeqCst);
    for step in steps {
        if let Err(err) = step.apply(writer) {
            let _ = leave(writer);
            return Err(err);
        }
        if step == TerminalStep::KeyboardEnhancement(true) {
            ENHANCEMENT_PUSHED.store(true, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// Restores the terminal; a no-op unless `enter` ran since the last call,
/// so the panic hook and the normal exit path can both call it. Keeps
/// going past a failed step so the later modes still get restored.
pub(crate) fn leave(writer: &mut impl Write) -> io::Result<()> {
    if !SCREEN_ENTERED.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    let enhancement = ENHANCEMENT_PUSHED.swap(false, Ordering::SeqCst);
    let mut first_error = None;
    for step in leave_sequence(enhancement) {
        if let Err(err) = step.apply(writer) {
            first_error.get_or_insert(err);
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// One frame bracketed by a synchronized update so the terminal paints it
/// atomically. Rows holding emoji or VS16/ZWJ graphemes are re-emitted
/// whole after ratatui's cell diff, since terminals disagree on how many
/// columns those take and a partial repaint leaves ghost cells behind.
pub(crate) fn draw_frame<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut ratatui::Frame),
) -> io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = draw_and_repaint_uncertain(terminal, render);
    let ended = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    drawn.and(ended)
}

/// Draws the frame, then writes every cell of each row `uncertain_rows`
/// picks straight to the backend. The hardware cursor is hidden for the
/// whole UI (nothing sets a cursor position), so the extra draw cannot
/// leave it somewhere ratatui does not expect.
fn draw_and_repaint_uncertain<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    render: impl FnOnce(&mut ratatui::Frame),
) -> Result<(), B::Error> {
    let cells = uncertain_rows(terminal.draw(render)?.buffer);
    if cells.is_empty() {
        return Ok(());
    }
    let backend = terminal.backend_mut();
    backend.draw(cells.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
    backend.flush()
}

/// Every cell, in x order, of each row whose symbols include an emoji
/// presentation sequence (U+FE0F), a ZWJ sequence (U+200D), a symbol from
/// the U+2600..=U+27BF blocks, or anything at U+1F000 and above. Cells
/// hidden under a wide grapheme are left out the way ratatui's own diff
/// leaves them out, so the repaint never splits a grapheme.
fn uncertain_rows(buffer: &Buffer) -> Vec<(u16, u16, Cell)> {
    let area = buffer.area;
    let mut cells = Vec::new();
    for y in area.top()..area.bottom() {
        let row = (area.left()..area.right()).map(|x| &buffer[(x, y)]);
        if !row
            .clone()
            .any(|cell| cell.symbol().chars().any(is_width_uncertain))
        {
            continue;
        }
        let mut hidden = 0u16;
        for (x, cell) in (area.left()..area.right()).zip(row) {
            if hidden > 0 {
                hidden -= 1;
                continue;
            }
            if matches!(cell.diff_option, CellDiffOption::Skip) {
                continue;
            }
            hidden = cell.cell_width().saturating_sub(1);
            cells.push((x, y, cell.clone()));
        }
    }
    cells
}

fn is_width_uncertain(c: char) -> bool {
    matches!(c, '\u{FE0F}' | '\u{200D}' | '\u{2600}'..='\u{27BF}') || c >= '\u{1F000}'
}

/// Clears the terminal and forgets both of ratatui's buffers so the next
/// `draw` repaints every cell. `Terminal::clear` is not used because it
/// starts with a cursor-position query, which blocks until the event
/// stream answers it, and nothing polls that stream here.
fn repaint_from_scratch<W: Write>(terminal: &mut Terminal<CrosstermBackend<W>>) -> io::Result<()> {
    execute!(terminal.backend_mut(), Clear(ClearType::All))?;
    terminal.current_buffer_mut().reset();
    terminal.swap_buffers();
    Ok(())
}

/// Re-applies the terminal modes and schedules a full repaint when `leave`
/// ran behind the UI's back: the panic hook restores the terminal for a
/// panic on a background task too, and that one is caught as a join error
/// while the loop keeps drawing. Returns whether it had to.
fn reenter_if_left<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    steps: Vec<TerminalStep>,
) -> io::Result<bool> {
    if SCREEN_ENTERED.load(Ordering::SeqCst) {
        return Ok(false);
    }
    warn!("The terminal was restored behind the UI; re-entering the screen");
    enter_steps(terminal.backend_mut(), steps)?;
    repaint_from_scratch(terminal)?;
    Ok(true)
}

/// The alternate screen the UI draws on, with the terminal modes it owns.
pub(crate) struct Screen {
    terminal: Terminal<CrosstermBackend<TtyWriter>>,
    enhancement: bool,
    #[cfg(unix)]
    fd: i32,
    /// The line discipline as it was before raw mode first came on. Raw
    /// mode is restored from crossterm's own snapshot, which it takes on
    /// the first `enable_raw_mode` after a `disable_raw_mode`; a child
    /// run with the terminal released may change the modes in between,
    /// so the re-entry puts these back first.
    #[cfg(unix)]
    cooked: libc::termios,
}

impl Screen {
    /// Probes the terminal, enters the alternate screen and builds the
    /// ratatui terminal over the dup'd stdout. The terminal is built
    /// first (it only asks for the size) so a failure after the modes are
    /// applied cannot leave them on.
    pub(crate) fn enter() -> Result<Self> {
        let enhancement = probe_keyboard_enhancement();
        let writer = open_tty_writer()?;
        #[cfg(unix)]
        let fd = std::os::fd::AsRawFd::as_raw_fd(&writer);
        #[cfg(unix)]
        let cooked = tty_modes(fd)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(writer))?;
        #[cfg(unix)]
        PANIC_FD.store(fd, Ordering::SeqCst);
        if let Err(err) = enter(terminal.backend_mut(), enhancement) {
            // Taken back before `terminal` drops the writer: the hook must
            // never write to a closed or reused descriptor.
            #[cfg(unix)]
            PANIC_FD.swap(-1, Ordering::SeqCst);
            return Err(err.into());
        }
        Ok(Self {
            terminal,
            enhancement,
            #[cfg(unix)]
            fd,
            #[cfg(unix)]
            cooked,
        })
    }

    /// Points the panic hook at this screen's descriptor again. Called
    /// after every re-enter so the hook's view never depends on which
    /// path last took the terminal.
    fn arm_panic_hook(&self) {
        #[cfg(unix)]
        PANIC_FD.store(self.fd, Ordering::SeqCst);
    }

    #[cfg(unix)]
    fn restore_cooked_modes(&self) -> io::Result<()> {
        set_tty_modes(self.fd, &self.cooked)
    }

    #[cfg(not(unix))]
    fn restore_cooked_modes(&self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn draw(&mut self, render: impl FnOnce(&mut ratatui::Frame)) -> io::Result<()> {
        if reenter_if_left(&mut self.terminal, enter_sequence(self.enhancement))? {
            self.arm_panic_hook();
        }
        draw_frame(&mut self.terminal, render)
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        repaint_from_scratch(&mut self.terminal)
    }

    pub(crate) fn leave(&mut self) -> io::Result<()> {
        leave(self.terminal.backend_mut())
    }

    /// Prints a line through the dup'd descriptor, for a message that has
    /// to reach the terminal while fds 1 and 2 are still captured by a
    /// turn that never finished. Meant for after `leave`.
    pub(crate) fn print_line(&mut self, text: &str) -> io::Result<()> {
        let writer = self.terminal.backend_mut();
        writeln!(writer, "{text}")?;
        writer.flush()
    }

    /// Hands the terminal to `f` in its normal modes (a suspended shell, an
    /// editor), then takes it back: puts the line discipline back to its
    /// pre-UI state so a child that changed it (an editor leaving echo
    /// off) leaves no trace in what the final `leave` restores, re-enters,
    /// and clears so the next frame repaints everything.
    pub(crate) fn with_terminal_released<T>(&mut self, f: impl FnOnce() -> T) -> io::Result<T> {
        self.leave()?;
        let value = f();
        if let Err(err) = self.restore_cooked_modes() {
            warn!("Failed to restore the terminal modes after the terminal was released: {err}");
        }
        enter(self.terminal.backend_mut(), self.enhancement)?;
        self.arm_panic_hook();
        repaint_from_scratch(&mut self.terminal)?;
        Ok(value)
    }
}

/// An early `?` or an unwind between `enter` and the explicit `leave` in
/// the run loop still restores the terminal; after that `leave` this is a
/// no-op. The hook's descriptor is forgotten here, ahead of the writer
/// closing it; a hook already past its `load` on another thread writes to
/// the closed fd and ignores the EBADF.
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.leave();
        #[cfg(unix)]
        PANIC_FD.swap(-1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::layout::{Position, Rect, Size};
    use ratatui::widgets::Paragraph;
    use ratatui::{TerminalOptions, Viewport};
    use serial_test::serial;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn take(&self) -> String {
            String::from_utf8_lossy(&std::mem::take(&mut *self.0.lock().unwrap())).into_owned()
        }
    }

    /// A terminal writing into `buf`; the fixed viewport keeps ratatui
    /// from asking the (absent) terminal for its size.
    fn buffer_terminal(buf: &SharedBuf) -> Terminal<CrosstermBackend<SharedBuf>> {
        Terminal::with_options(
            CrosstermBackend::new(buf.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 20, 4)),
            },
        )
        .unwrap()
    }

    /// A writer that fails its `n`th write and records the rest.
    struct FailingWriter {
        out: Vec<u8>,
        writes: usize,
        fail_at: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == self.fail_at {
                return Err(io::Error::other("write refused"));
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The enter steps that only write to the terminal: raw mode is left
    /// out because it would switch the test runner's own tty.
    fn writing_enter_steps() -> Vec<TerminalStep> {
        enter_sequence(false)
            .into_iter()
            .filter(|step| !matches!(step, TerminalStep::RawMode(_)))
            .collect()
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    #[test]
    fn leave_sequence_is_the_strict_mirror_of_enter() {
        for enhancement in [false, true] {
            let enter = enter_sequence(enhancement);
            let leave = leave_sequence(enhancement);
            assert_eq!(enter.len(), leave.len());
            let mirrored: Vec<TerminalStep> =
                leave.iter().rev().map(|step| step.inverse()).collect();
            assert_eq!(mirrored, enter);
            assert_eq!(enter[0], TerminalStep::RawMode(true));
            assert_eq!(enter[1], TerminalStep::AlternateScreen(true));
            assert_eq!(enter[2], TerminalStep::CursorHidden(true));
            assert_eq!(enter[3], TerminalStep::BracketedPaste(true));
            assert_eq!(*leave.last().unwrap(), TerminalStep::RawMode(false));
        }
        assert_eq!(
            *enter_sequence(true).last().unwrap(),
            TerminalStep::KeyboardEnhancement(true)
        );
        assert!(
            !enter_sequence(false)
                .iter()
                .any(|step| matches!(step, TerminalStep::KeyboardEnhancement(_)))
        );
    }

    /// Without an `enter` on record there is nothing to undo, so `leave`
    /// writes no bytes and a second call is just as quiet.
    #[test]
    #[serial]
    fn leave_without_a_prior_enter_is_a_no_op_twice() {
        SCREEN_ENTERED.store(false, Ordering::SeqCst);
        let mut out = Vec::new();
        leave(&mut out).unwrap();
        leave(&mut out).unwrap();
        assert!(out.is_empty(), "{out:?}");
    }

    /// A step failing midway hands back what came on before it: the
    /// alternate screen entered by the first write is left again, the
    /// flag is down, and nothing is pushed.
    #[test]
    #[serial]
    fn a_failing_enter_step_unwinds_the_steps_already_applied() {
        SCREEN_ENTERED.store(false, Ordering::SeqCst);
        ENHANCEMENT_PUSHED.store(false, Ordering::SeqCst);
        let mut writer = FailingWriter {
            out: Vec::new(),
            writes: 0,
            fail_at: 2,
        };
        let err = enter_steps(&mut writer, writing_enter_steps()).unwrap_err();
        assert_eq!(err.to_string(), "write refused");
        assert!(!SCREEN_ENTERED.load(Ordering::SeqCst));
        assert!(!ENHANCEMENT_PUSHED.load(Ordering::SeqCst));
        let enter_alt = find(&writer.out, b"\x1b[?1049h").expect("the first step was applied");
        let leave_alt = find(&writer.out, b"\x1b[?1049l").expect("the applied step was undone");
        assert!(enter_alt < leave_alt, "{:?}", writer.out);
        assert!(
            !writer.out.contains(&b'u'),
            "no enhancement flags were pushed or popped: {:?}",
            writer.out
        );
    }

    /// `leave` undoes an `enter` once: the full mirror on the first call
    /// (pop, then the modes, then the cursor shown inside the alternate
    /// screen before it is left), nothing on the second.
    #[test]
    #[serial]
    fn leave_restores_once_and_is_silent_afterwards() {
        SCREEN_ENTERED.store(true, Ordering::SeqCst);
        ENHANCEMENT_PUSHED.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        leave(&mut out).unwrap();
        assert_eq!(count(&out, b"\x1b[?1049l"), 1, "{out:?}");
        assert_eq!(count(&out, b"\x1b[<1u"), 1, "{out:?}");
        assert!(find(&out, b"\x1b[<1u") < find(&out, b"\x1b[?1049l"));
        assert_eq!(count(&out, b"\x1b[?25h"), 1, "{out:?}");
        assert!(find(&out, b"\x1b[?25h") < find(&out, b"\x1b[?1049l"));
        assert!(!SCREEN_ENTERED.load(Ordering::SeqCst));
        assert!(!ENHANCEMENT_PUSHED.load(Ordering::SeqCst));

        let mut again = Vec::new();
        leave(&mut again).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    /// A frame drawn after something else restored the terminal first
    /// re-enters it and repaints from scratch; once back, drawing is just
    /// drawing.
    #[test]
    #[serial]
    fn a_draw_after_a_foreign_leave_reenters_and_repaints_once() {
        SCREEN_ENTERED.store(false, Ordering::SeqCst);
        let out = SharedBuf::default();
        let mut terminal = buffer_terminal(&out);
        assert!(reenter_if_left(&mut terminal, writing_enter_steps()).unwrap());
        let bytes = out.take();
        assert!(bytes.contains("\x1b[?1049h"), "{bytes:?}");
        assert!(bytes.contains("\x1b[2J"), "{bytes:?}");
        assert!(SCREEN_ENTERED.load(Ordering::SeqCst));

        assert!(!reenter_if_left(&mut terminal, writing_enter_steps()).unwrap());
        assert!(out.take().is_empty());
        SCREEN_ENTERED.store(false, Ordering::SeqCst);
    }

    /// The repaint helper never asks the terminal anything: a buffer that
    /// cannot answer a cursor-position query gets the clear and the next
    /// frame paints every cell, including the ones unchanged since before.
    #[test]
    fn repaint_from_scratch_clears_and_repaints_every_cell_without_a_query() {
        let out = SharedBuf::default();
        let mut terminal = buffer_terminal(&out);
        let paint = |frame: &mut ratatui::Frame| {
            frame.render_widget(Paragraph::new("cell-x7x"), frame.area());
        };
        terminal.draw(paint).unwrap();
        out.take();
        terminal.draw(paint).unwrap();
        assert!(
            !out.take().contains("cell-x7x"),
            "an unchanged frame should paint nothing"
        );

        repaint_from_scratch(&mut terminal).unwrap();
        let bytes = out.take();
        assert!(bytes.contains("\x1b[2J"), "{bytes:?}");
        assert!(!bytes.contains("\x1b[6n"), "{bytes:?}");
        terminal.draw(paint).unwrap();
        assert!(
            out.take().contains("cell-x7x"),
            "the frame after a repaint must paint every cell"
        );
    }

    #[test]
    fn a_frame_is_bracketed_by_a_synchronized_update() {
        let out = SharedBuf::default();
        let mut terminal = buffer_terminal(&out);
        draw_frame(&mut terminal, |frame| {
            frame.render_widget(Paragraph::new("x"), frame.area());
        })
        .unwrap();
        let bytes = out.take();
        assert!(bytes.starts_with("\x1b[?2026h"), "{bytes:?}");
        assert!(bytes.ends_with("\x1b[?2026l"), "{bytes:?}");
    }

    fn positions(cells: &[(u16, u16, Cell)]) -> Vec<(u16, u16)> {
        cells.iter().map(|(x, y, _)| (*x, *y)).collect()
    }

    /// Every column of row `y` except the ones hidden under the wide
    /// grapheme at `wide_x`.
    fn whole_row(buffer: &Buffer, y: u16, wide_x: u16) -> Vec<(u16, u16)> {
        let hidden = wide_x + 1..wide_x + buffer[(wide_x, y)].cell_width();
        (0..buffer.area.width)
            .filter(|x| !hidden.contains(x))
            .map(|x| (x, y))
            .collect()
    }

    #[test]
    fn plain_rows_are_never_uncertain() {
        let buffer = Buffer::with_lines(["plain text", "more text "]);
        assert!(uncertain_rows(&buffer).is_empty());
    }

    #[test]
    fn a_vs16_grapheme_marks_its_whole_row_in_x_order() {
        let buffer = Buffer::with_lines(["ok \u{2714}\u{fe0f} done", "plain     "]);
        let cells = uncertain_rows(&buffer);
        assert_eq!(positions(&cells), whole_row(&buffer, 0, 3));
        assert_eq!(cells[3].2.symbol(), "\u{2714}\u{fe0f}");
    }

    #[test]
    fn a_zwj_sequence_marks_its_whole_row() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let buffer = Buffer::with_lines([format!("{family} fam"), "plain ".to_string()]);
        let cells = uncertain_rows(&buffer);
        assert_eq!(positions(&cells), whole_row(&buffer, 0, 0));
        assert_eq!(cells[0].2.symbol(), family);
    }

    #[test]
    fn every_uncertain_row_is_marked() {
        let buffer = Buffer::with_lines(["\u{1F600} hi", "plain", "\u{2705} ok"]);
        let cells = uncertain_rows(&buffer);
        let mut expected = whole_row(&buffer, 0, 0);
        expected.extend(whole_row(&buffer, 2, 0));
        assert_eq!(positions(&cells), expected);
    }

    /// A backend that keeps every `draw` it receives, so a test can tell
    /// what ratatui's diff emitted from what the repaint added.
    struct RecordingBackend {
        inner: TestBackend,
        draws: Vec<Vec<(u16, u16, Cell)>>,
    }

    impl Backend for RecordingBackend {
        type Error = std::convert::Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            let cells: Vec<_> = content.map(|(x, y, c)| (x, y, c.clone())).collect();
            self.inner.draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))?;
            self.draws.push(cells);
            Ok(())
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
            self.inner.append_lines(n)
        }

        fn size(&self) -> Result<Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.inner.flush()
        }
    }

    /// The frame after a one-cell change in each row: ratatui's diff emits
    /// the two changed cells, and the repaint adds the rest of the emoji
    /// row and nothing from the plain row.
    #[test]
    fn a_changed_emoji_row_is_repainted_whole_and_a_plain_row_is_not() {
        let mut terminal = Terminal::new(RecordingBackend {
            inner: TestBackend::new(12, 2),
            draws: Vec::new(),
        })
        .unwrap();
        let paint = |lines: [&'static str; 2]| {
            move |frame: &mut ratatui::Frame| {
                frame.render_widget(Paragraph::new(lines.join("\n")), frame.area());
            }
        };
        draw_and_repaint_uncertain(&mut terminal, paint(["\u{1F600} smile ok", "plain row"]))
            .unwrap();
        let first_frame_draws = terminal.backend().draws.len();

        draw_and_repaint_uncertain(&mut terminal, paint(["\u{1F600} smilE ok", "plain roW"]))
            .unwrap();

        let draws = &terminal.backend().draws[first_frame_draws..];
        let diff = positions(&draws[0]);
        assert_eq!(diff, vec![(7, 0), (8, 1)]);
        let repaint = positions(&draws[1]);
        assert_eq!(repaint, whole_row(terminal.backend().inner.buffer(), 0, 0));
        assert_eq!(draws.len(), 2);
        terminal
            .backend()
            .inner
            .assert_buffer_lines(["\u{1F600} smilE ok", "plain roW   "]);
    }

    #[test]
    fn panic_hook_installs_once() {
        install_panic_hook();
        assert!(PANIC_HOOK.is_completed());
        assert!(!install_panic_hook());
    }

    /// A panic restores the terminal, the UI re-enters it, and a second
    /// panic restores it again: the hook's descriptor survives the first
    /// restore.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn the_panic_hook_restores_again_after_a_reenter() {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        // Every leave step flushes before the hook returns and the alternate
        // screen is the last one written, so reading up to it cannot block.
        fn read_through_leave_alt(reader: &mut io::PipeReader) -> Vec<u8> {
            let mut out = Vec::new();
            let mut chunk = [0u8; 256];
            while !out.ends_with(b"\x1b[?1049l") {
                let n = reader.read(&mut chunk).unwrap();
                assert_ne!(n, 0, "the pipe closed before the leave sequence: {out:?}");
                out.extend_from_slice(&chunk[..n]);
            }
            out
        }

        let (mut reader, writer) = io::pipe().unwrap();
        PANIC_FD.store(writer.as_raw_fd(), Ordering::SeqCst);
        ENHANCEMENT_PUSHED.store(false, Ordering::SeqCst);

        SCREEN_ENTERED.store(true, Ordering::SeqCst);
        restore_from_static();
        let first = read_through_leave_alt(&mut reader);
        assert_eq!(count(&first, b"\x1b[?1049l"), 1, "{first:?}");
        assert!(!SCREEN_ENTERED.load(Ordering::SeqCst));
        assert_eq!(
            PANIC_FD.load(Ordering::SeqCst),
            writer.as_raw_fd(),
            "the first restore disarmed the hook"
        );

        // The UI re-entering the screen, as `reenter_if_left` does.
        SCREEN_ENTERED.store(true, Ordering::SeqCst);
        restore_from_static();
        let second = read_through_leave_alt(&mut reader);
        assert_eq!(count(&second, b"\x1b[?1049l"), 1, "{second:?}");
        assert!(!SCREEN_ENTERED.load(Ordering::SeqCst));

        PANIC_FD.store(-1, Ordering::SeqCst);
    }
}
