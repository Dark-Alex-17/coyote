//! Terminal lifecycle for the full-screen UI. Every escape sequence goes
//! through a descriptor dup'd from stdout once at start, so a command turn
//! that redirects fd 1 into its capture pipe never swallows a draw or the
//! restore, and the panic hook writes the leave sequence through that same
//! descriptor before the default hook prints.

use anyhow::Result;
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
/// `Screen::enter` has run.
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

/// Installs the restoring panic hook in front of whatever hook is current.
/// Returns whether this call installed it; later calls are no-ops.
pub(crate) fn install_panic_hook() -> bool {
    let mut installed = false;
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_from_static();
            previous(info);
        }));
        installed = true;
    });
    installed
}

/// Writes the leave sequence through the descriptor `enter` recorded. Runs
/// on the panic path, so every error is ignored: there is nothing left to
/// report it to.
fn restore_from_static() {
    #[cfg(unix)]
    {
        use std::mem::ManuallyDrop;
        use std::os::fd::FromRawFd;
        let fd = PANIC_FD.load(Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        // SAFETY: the fd was dup'd by `open_tty_writer` and stays open for
        // the process lifetime; ManuallyDrop keeps this handle from closing it.
        let mut writer = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
        let _ = leave(&mut *writer);
    }
    #[cfg(not(unix))]
    {
        let _ = leave(&mut io::stdout());
    }
}

/// Switches the terminal into the UI's modes.
pub(crate) fn enter(writer: &mut impl Write, enhancement: bool) -> io::Result<()> {
    install_panic_hook();
    SCREEN_ENTERED.store(true, Ordering::SeqCst);
    for step in enter_sequence(enhancement) {
        step.apply(writer)?;
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
/// atomically.
pub(crate) fn draw_frame<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut ratatui::Frame),
) -> io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = terminal.draw(render).map(|_| ());
    let ended = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    drawn.and(ended)
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

/// The alternate screen the UI draws on, with the terminal modes it owns.
pub(crate) struct Screen {
    terminal: Terminal<CrosstermBackend<TtyWriter>>,
    enhancement: bool,
}

impl Screen {
    /// Probes the terminal, enters the alternate screen and builds the
    /// ratatui terminal over the dup'd stdout.
    pub(crate) fn enter() -> Result<Self> {
        let enhancement = probe_keyboard_enhancement();
        let mut writer = open_tty_writer()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            PANIC_FD.store(writer.as_raw_fd(), Ordering::SeqCst);
        }
        enter(&mut writer, enhancement)?;
        let terminal = Terminal::new(CrosstermBackend::new(writer))?;
        Ok(Self {
            terminal,
            enhancement,
        })
    }

    pub(crate) fn draw(&mut self, render: impl FnOnce(&mut ratatui::Frame)) -> io::Result<()> {
        draw_frame(&mut self.terminal, render)
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        repaint_from_scratch(&mut self.terminal)
    }

    pub(crate) fn leave(&mut self) -> io::Result<()> {
        leave(self.terminal.backend_mut())
    }

    /// Hands the terminal to `f` in its normal modes (a suspended shell, an
    /// editor), then takes it back: re-enters, re-applies raw mode so a
    /// child that changed the termios state leaves no trace, and clears so
    /// the next frame repaints everything.
    pub(crate) fn with_terminal_released<T>(&mut self, f: impl FnOnce() -> T) -> io::Result<T> {
        self.leave()?;
        let value = f();
        enter(self.terminal.backend_mut(), self.enhancement)?;
        disable_raw_mode()?;
        enable_raw_mode()?;
        repaint_from_scratch(&mut self.terminal)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::widgets::Paragraph;
    use ratatui::{TerminalOptions, Viewport};
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
            assert_eq!(enter[2], TerminalStep::BracketedPaste(true));
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
    fn leave_without_a_prior_enter_is_a_no_op_twice() {
        SCREEN_ENTERED.store(false, Ordering::SeqCst);
        let mut out = Vec::new();
        leave(&mut out).unwrap();
        leave(&mut out).unwrap();
        assert!(out.is_empty(), "{out:?}");
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

    #[test]
    fn panic_hook_installs_once() {
        install_panic_hook();
        assert!(PANIC_HOOK.is_completed());
        assert!(!install_panic_hook());
    }
}
