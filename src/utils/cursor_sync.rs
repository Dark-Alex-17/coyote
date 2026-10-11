//! crossterm hands `cursor::position()` the OLDEST queued reply, and every
//! query appends one reply behind it, so once a reply is orphaned every later
//! query stays one behind: querying can never shrink the lag. A pop consumes a
//! reply without asking (stdout muted), which is the only way to catch up.
//! Accepted residual: a stale reply sitting at the first sentinel column yields
//! a false `Synced { drained: 0 }` that self-heals on the next call provided
//! the caller draws a fresh `start` via `next_sentinel_start()`; the rotation
//! makes that recur at most once per `PREFERRED.len()` calls. Consecutive
//! calls share no sentinel column on terminals of 24 columns or more, where
//! every `PREFERRED` entry fits; narrower panes drop the high columns and the
//! windows may overlap again.
//! This assumes coyote and reedline share ONE crossterm instance (Cargo.lock:
//! both on crossterm 0.29.0); a reedline bump to a different crossterm major
//! would silently split the queues.

use std::{
    io,
    sync::atomic::{AtomicUsize, Ordering},
};

#[cfg(unix)]
use std::{
    ffi::OsStr,
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Outcome of draining stale cursor-position replies from crossterm's event queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resync {
    Skipped,
    Synced {
        drained: usize,
    },
    /// The first query or a confirm query timed out, or a pop found the queue
    /// empty: our in-flight reply never arrived and the next call drains it as
    /// a mismatch.
    Unanswered,
    /// The pop budget ran out, no fresh column was left, or a move failed to
    /// write; the backoff treats all three as neutral.
    GaveUp {
        drained: usize,
    },
}

/// Budget of queue entries consumed per call, by query or by pop.
pub(crate) const MAX_POPS: usize = 24;
/// Matching queries required after a pop match, which may be a coincidence.
const CONFIRMS: usize = 2;
// Need at least three fresh columns in 1..cols: one sentinel plus the confirms.
const MIN_COLS: u16 = 4;
const PREFERRED: [u16; 7] = [5, 8, 11, 14, 17, 20, 23];
// A clean or single-stall call uses at most three consecutive entries, so
// stepping by 3 keeps consecutive calls' columns disjoint; 3 is coprime with
// 7, so every start still cycles.
pub(crate) const START_STEP: usize = 3;

static NEXT_START: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn next_sentinel_start() -> usize {
    NEXT_START.fetch_add(START_STEP, Ordering::Relaxed) % PREFERRED.len()
}

/// Sentinel columns: `PREFERRED` rotated to begin at `start`, then `1..cols`.
/// Never yields 0, a column at or past `cols`, a column already yielded, or
/// one in `seen`.
struct Fresh {
    cols: u16,
    start: usize,
    next_index: usize,
    yielded: Vec<u16>,
}

impl Fresh {
    fn new(cols: u16, start: usize) -> Self {
        Self {
            cols,
            start: start % PREFERRED.len(),
            next_index: 0,
            yielded: vec![],
        }
    }

    fn candidate(&self, index: usize) -> Option<u16> {
        match index.checked_sub(PREFERRED.len()) {
            None => Some(PREFERRED[(self.start + index) % PREFERRED.len()]),
            Some(offset) => u16::try_from(offset + 1)
                .ok()
                .filter(|col| *col < self.cols),
        }
    }

    fn next(&mut self, seen: &[u16]) -> Option<u16> {
        loop {
            let col = self.candidate(self.next_index)?;
            self.next_index += 1;
            if col < self.cols && !self.yielded.contains(&col) && !seen.contains(&col) {
                self.yielded.push(col);
                return Some(col);
            }
        }
    }
}

/// Moves the cursor to a sentinel column and queries; on a clean queue that
/// single round-trip is the whole call. A mismatch proves the queue dirty and
/// our reply the newest entry, so stale replies are popped until it surfaces,
/// then `CONFIRMS` fresh-column queries rule out a coincidental match. Any
/// mismatch during the confirms resumes popping. `mv(col)` must queue
/// `MoveToColumn(col)` and flush; `query()` is `cursor::position()`; `pop()`
/// consumes one queued reply without asking and fails when none is queued.
pub fn resync(
    cols: u16,
    start: usize,
    mut mv: impl FnMut(u16) -> io::Result<()>,
    mut query: impl FnMut() -> io::Result<(u16, u16)>,
    mut pop: impl FnMut() -> io::Result<(u16, u16)>,
) -> Resync {
    if cols < MIN_COLS {
        return Resync::Skipped;
    }
    let mut fresh = Fresh::new(cols, start);
    let mut seen: Vec<u16> = vec![];
    let mut drained = 0;
    let mut budget = MAX_POPS;

    let Some(mut s) = fresh.next(&seen) else {
        return Resync::GaveUp { drained };
    };
    if mv(s).is_err() {
        return Resync::GaveUp { drained };
    }
    budget -= 1;
    match query() {
        Err(_) => return Resync::Unanswered,
        Ok((c, _)) if c == s => return Resync::Synced { drained: 0 },
        Ok((c, _)) => {
            seen.push(c);
            drained = 1;
        }
    }

    let mut confirmed = 0;
    while budget > 0 {
        budget -= 1;
        let reply = if confirmed == 0 {
            pop()
        } else {
            let Some(next) = fresh.next(&seen) else {
                return Resync::GaveUp { drained };
            };
            s = next;
            if mv(s).is_err() {
                return Resync::GaveUp { drained };
            }
            query()
        };
        match reply {
            Err(_) => return Resync::Unanswered,
            Ok((c, _)) if c != s => {
                seen.push(c);
                drained += 1;
                confirmed = 0;
            }
            Ok(_) => {
                if confirmed == CONFIRMS {
                    return Resync::Synced { drained };
                }
                confirmed += 1;
            }
        }
    }
    Resync::GaveUp { drained }
}

#[cfg(unix)]
struct MutedStdout {
    _lock: io::StdoutLock<'static>,
    saved: OwnedFd,
}

// The lock is reentrant, so `position()`'s own stdout writes on this thread
// still land (in /dev/null) while other threads block instead of losing bytes.
#[cfg(unix)]
fn mute_stdout() -> io::Result<MutedStdout> {
    let mut lock = io::stdout().lock();
    lock.flush()?;
    let null = File::options().write(true).open("/dev/null")?;
    // SAFETY: fcntl with F_DUPFD_CLOEXEC takes no pointers; the result is checked below.
    let saved = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if saved == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `saved` was just created by fcntl and nothing else owns it.
    let saved = unsafe { OwnedFd::from_raw_fd(saved) };
    // SAFETY: both descriptors are open; dup2 takes no pointers.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(MutedStdout { _lock: lock, saved })
}

#[cfg(unix)]
impl Drop for MutedStdout {
    fn drop(&mut self) {
        loop {
            // SAFETY: both descriptors are open; dup2 takes no pointers.
            let rc = unsafe { libc::dup2(self.saved.as_raw_fd(), libc::STDOUT_FILENO) };
            if rc != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
    }
}

/// Consumes the oldest queued cursor-position reply without issuing a query:
/// the `ESC[6n` goes to /dev/null. Blocks up to 2 s and fails when none is
/// queued. Raw mode must already be enabled or `position()` would toggle it.
#[cfg(unix)]
pub(crate) fn pop_stale() -> io::Result<(u16, u16)> {
    let _muted = mute_stdout()?;
    crossterm::cursor::position()
}

#[cfg(not(unix))]
pub(crate) fn pop_stale() -> io::Result<(u16, u16)> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(unix)]
const BACKOFF_FIRST: Duration = Duration::from_secs(30);
#[cfg(unix)]
const BACKOFF_CAP: Duration = Duration::from_secs(300);
// reedline already pays the same 2 s per prompt on a terminal that never
// answers, so three extra hits before backing off is the accepted cost.
#[cfg(unix)]
const UNANSWERED_BEFORE_BACKOFF: u32 = 3;

#[cfg(unix)]
static BACKOFF: Mutex<Backoff> = Mutex::new(Backoff::new());

#[cfg(unix)]
fn next_backoff(prev: Option<Duration>) -> Duration {
    match prev {
        None => BACKOFF_FIRST,
        Some(prev) => (prev * 2).min(BACKOFF_CAP),
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct Backoff {
    unanswered: u32,
    armed: Option<(Instant, Duration)>,
}

#[cfg(unix)]
impl Backoff {
    const fn new() -> Self {
        Self {
            unanswered: 0,
            armed: None,
        }
    }

    fn armed(&self, now: Instant) -> bool {
        self.armed.is_some_and(|(until, _)| now < until)
    }

    fn record(&mut self, outcome: Resync, now: Instant) {
        match outcome {
            Resync::Synced { .. } => *self = Self::new(),
            Resync::Unanswered => {
                self.unanswered = self.unanswered.saturating_add(1);
                if self.unanswered >= UNANSWERED_BEFORE_BACKOFF {
                    let wait = next_backoff(self.armed.map(|(_, prev)| prev));
                    self.armed = Some((now + wait, wait));
                }
            }
            Resync::Skipped | Resync::GaveUp { .. } => {}
        }
    }
}

#[cfg(unix)]
pub(crate) fn backoff_armed() -> bool {
    BACKOFF
        .lock()
        .ok()
        .is_some_and(|backoff| backoff.armed(Instant::now()))
}

#[cfg(not(unix))]
pub(crate) fn backoff_armed() -> bool {
    false
}

#[cfg(unix)]
fn term_is_dumb(term: Option<&OsStr>) -> bool {
    term.is_none_or(|term| term.is_empty() || term == "dumb")
}

/// Resyncs the terminal's cursor-position replies on stdout, leaving the
/// cursor position and raw mode as they were found. Backs off only after
/// three consecutive unanswered queries: a single late answer is exactly the
/// orphan this resync exists to drain, and draining it is free on the next
/// call, so it must not disable the resync.
#[cfg(unix)]
pub fn resync_cursor_queries() -> Resync {
    let outcome = resync_stdout();
    match outcome {
        Resync::Synced { drained: 0 } => trace!("cursor resync: {outcome:?}"),
        _ => debug!("cursor resync: {outcome:?}"),
    }
    outcome
}

#[cfg(not(unix))]
pub fn resync_cursor_queries() -> Resync {
    Resync::Skipped
}

#[cfg(unix)]
fn should_skip(
    stdout_tty: bool,
    stdin_tty: bool,
    term: Option<&OsStr>,
    backoff_armed: bool,
) -> bool {
    !stdout_tty || !stdin_tty || term_is_dumb(term) || backoff_armed
}

#[cfg(unix)]
trait RawMode {
    fn is_enabled(&self) -> bool;
    fn enable(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
}

#[cfg(unix)]
struct CrosstermRawMode;

#[cfg(unix)]
impl RawMode for CrosstermRawMode {
    fn is_enabled(&self) -> bool {
        crossterm::terminal::is_raw_mode_enabled().unwrap_or(false)
    }

    fn enable(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn disable(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
}

#[cfg(unix)]
fn resync_stdout() -> Resync {
    use crate::utils::IS_STDOUT_TERMINAL;
    use is_terminal::IsTerminal;

    if should_skip(
        *IS_STDOUT_TERMINAL,
        io::stdin().is_terminal(),
        std::env::var_os("TERM").as_deref(),
        backoff_armed(),
    ) {
        return Resync::Skipped;
    }
    let Ok((cols, _)) = crossterm::terminal::size() else {
        return Resync::Skipped;
    };
    let outcome = resync_on(
        &mut io::stdout(),
        cols,
        next_sentinel_start(),
        &mut CrosstermRawMode,
        crossterm::cursor::position,
        pop_stale,
    );
    if let Ok(mut backoff) = BACKOFF.lock() {
        backoff.record(outcome, Instant::now());
    }
    outcome
}

#[cfg(unix)]
fn resync_on<W: Write>(
    w: &mut W,
    cols: u16,
    start: usize,
    raw: &mut impl RawMode,
    query: impl FnMut() -> io::Result<(u16, u16)>,
    pop: impl FnMut() -> io::Result<(u16, u16)>,
) -> Resync {
    use crossterm::{cursor, queue};

    // A reply arriving in cooked mode is echoed and held in the line buffer.
    let was_raw = raw.is_enabled();
    if !was_raw && raw.enable().is_err() {
        return Resync::Skipped;
    }

    // reedline anchors on the next row when the cursor is not at column 0 to
    // preserve a partial last line; restoring the column keeps that contract.
    let _ = queue!(w, cursor::SavePosition).and_then(|_| w.flush());
    let outcome = resync(
        cols,
        start,
        |col| {
            queue!(w, cursor::MoveToColumn(col))?;
            w.flush()
        },
        query,
        pop,
    );
    let _ = queue!(w, cursor::RestorePosition).and_then(|_| w.flush());
    if !was_raw {
        let _ = raw.disable();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    const ROW: u16 = 9;

    fn timeout() -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, "no reply")
    }

    // FIFO model of crossterm's reply queue: a query appends the terminal's
    // reply to the back and hands out the front, exactly one behind per orphan.
    struct Term {
        col: u16,
        honours_moves: bool,
        moves_allowed: usize,
        queue: VecDeque<(u16, u16)>,
        late: Vec<(u16, u16)>,
        stall_queries: usize,
        stall_from_query: usize,
        queries: usize,
        pops: usize,
        moves: Vec<u16>,
    }

    impl Term {
        fn new(stale: &[(u16, u16)]) -> Self {
            Self {
                col: 0,
                honours_moves: true,
                moves_allowed: usize::MAX,
                queue: stale.iter().copied().collect(),
                late: vec![],
                stall_queries: 0,
                stall_from_query: 0,
                queries: 0,
                pops: 0,
                moves: vec![],
            }
        }

        fn query(&mut self) -> io::Result<(u16, u16)> {
            self.queries += 1;
            let reply = (if self.honours_moves { self.col } else { 0 }, ROW);
            if self.stall_queries > 0 && self.queries > self.stall_from_query {
                self.stall_queries -= 1;
                self.late.push(reply);
            } else {
                self.queue.push_back(reply);
            }
            self.queue.pop_front().ok_or_else(timeout)
        }

        fn pop(&mut self) -> io::Result<(u16, u16)> {
            self.pops += 1;
            self.queue.pop_front().ok_or_else(timeout)
        }

        fn deliver_late(&mut self) {
            self.queue.extend(self.late.drain(..));
        }

        fn resync(&mut self, cols: u16, start: usize) -> Resync {
            let term = RefCell::new(self);
            resync(
                cols,
                start,
                |col| {
                    let mut term = term.borrow_mut();
                    if term.moves.len() == term.moves_allowed {
                        return Err(io::Error::other("closed"));
                    }
                    term.moves.push(col);
                    term.col = col;
                    Ok(())
                },
                || term.borrow_mut().query(),
                || term.borrow_mut().pop(),
            )
        }

        fn tally(&mut self) -> (usize, usize) {
            (
                std::mem::take(&mut self.queries),
                std::mem::take(&mut self.pops),
            )
        }
    }

    #[test]
    fn clean_queue_syncs_on_first_reply() {
        let mut term = Term::new(&[]);
        assert_eq!(term.resync(80, 0), Resync::Synced { drained: 0 });
        assert_eq!(term.tally(), (1, 0));
        assert_eq!(term.moves, [5]);
    }

    #[test]
    fn lag_one_pops_once_then_confirms_twice() {
        let mut term = Term::new(&[(0, 7)]);
        assert_eq!(term.resync(80, 0), Resync::Synced { drained: 1 });
        assert_eq!(term.tally(), (3, 1));
        assert_eq!(term.moves, [5, 8, 11]);
    }

    #[test]
    fn identical_orphans_are_all_popped() {
        let mut term = Term::new(&[(0, 7); 6]);
        assert_eq!(term.resync(80, 0), Resync::Synced { drained: 6 });
        assert_eq!(term.tally(), (3, 6));
    }

    #[test]
    fn orphans_at_the_start_column_pass_as_clean_until_start_rotates() {
        let mut term = Term::new(&[(5, 7); 6]);
        assert_eq!(term.resync(80, 0), Resync::Synced { drained: 0 });
        assert_eq!(term.tally(), (1, 0));

        let mut term = Term::new(&[(5, 7); 6]);
        assert_eq!(term.resync(80, 1), Resync::Synced { drained: 6 });
        assert_eq!(term.tally(), (3, 6));
        assert_eq!(term.moves, [8, 11, 14]);
    }

    // The pop match on the stale 5 is a coincidence; the confirm at 8 pops
    // our own (5, 9), which mismatches and resumes popping.
    #[test]
    fn single_coincidence_is_caught_by_the_confirm() {
        let mut term = Term::new(&[(0, 7), (5, 7)]);
        assert_eq!(term.resync(80, 0), Resync::Synced { drained: 2 });
        assert_eq!(term.tally(), (4, 2));
        assert_eq!(term.moves, [5, 8, 11, 14]);
    }

    // Pop A(5) matches, confirm@8 pops B(8) and matches too, confirm@11 pops
    // our (5, 9) and mismatches: popping resumes at s=11, drains our (8, 9),
    // matches our (11, 9), then confirms at 14 and 17. A single-confirm or
    // alternating design returns a false Synced here with two of our replies
    // still queued.
    #[test]
    fn double_coincidence_is_caught_by_the_second_confirm() {
        let mut term = Term::new(&[(0, 7), (5, 7), (8, 7)]);
        assert_eq!(term.resync(80, 0), Resync::Synced { drained: 3 });
        assert_eq!(term.tally(), (5, 3));
        assert_eq!(term.moves, [5, 8, 11, 14, 17]);
    }

    #[test]
    fn terminal_ignoring_column_moves_is_unanswered_after_one_pop() {
        let mut term = Term::new(&[]);
        term.honours_moves = false;
        assert_eq!(term.resync(80, 0), Resync::Unanswered);
        assert_eq!(term.tally(), (1, 1));
    }

    #[test]
    fn stalled_first_query_is_drained_by_the_next_call() {
        let mut term = Term::new(&[]);
        term.stall_queries = 1;
        assert_eq!(term.resync(80, 0), Resync::Unanswered);
        assert_eq!(term.tally(), (1, 0));
        assert_eq!(term.late, [(5, ROW)]);

        term.deliver_late();
        assert_eq!(term.resync(80, 1), Resync::Synced { drained: 1 });
        assert_eq!(term.tally(), (3, 1));
    }

    #[test]
    fn stall_during_drain_times_out_on_the_pop() {
        let mut term = Term::new(&[(0, 7)]);
        term.stall_queries = 1;
        assert_eq!(term.resync(80, 0), Resync::Unanswered);
        assert_eq!(term.tally(), (1, 1));
        assert_eq!(term.late, [(5, ROW)]);

        term.deliver_late();
        assert_eq!(term.resync(80, 1), Resync::Synced { drained: 1 });
    }

    #[test]
    fn budget_counts_queries_and_pops_together() {
        let stale: Vec<(u16, u16)> = (40..70).map(|col| (col, 7)).collect();
        let mut term = Term::new(&stale);
        assert_eq!(term.resync(80, 0), Resync::GaveUp { drained: MAX_POPS });
        assert_eq!(term.tally(), (1, MAX_POPS - 1));
    }

    #[test]
    fn narrow_terminals_fall_back_to_low_columns_or_skip() {
        let mut term = Term::new(&[]);
        assert_eq!(term.resync(3, 0), Resync::Skipped);
        assert_eq!(term.tally(), (0, 0));

        let mut term = Term::new(&[(0, 7)]);
        assert_eq!(term.resync(4, 0), Resync::Synced { drained: 1 });
        assert_eq!(term.tally(), (3, 1));
        assert_eq!(term.moves, [1, 2, 3]);

        let mut term = Term::new(&[(0, 7), (1, 7)]);
        assert_eq!(term.resync(4, 0), Resync::GaveUp { drained: 2 });
    }

    #[test]
    fn failed_move_gives_up_without_further_traffic() {
        let mut term = Term::new(&[]);
        term.moves_allowed = 0;
        assert_eq!(term.resync(80, 0), Resync::GaveUp { drained: 0 });
        assert_eq!(term.tally(), (0, 0));

        let mut term = Term::new(&[(0, 7)]);
        term.moves_allowed = 1;
        assert_eq!(term.resync(80, 0), Resync::GaveUp { drained: 1 });
        assert_eq!(term.tally(), (1, 1));
        assert_eq!(term.moves, [5]);
    }

    #[test]
    fn fresh_rotates_skips_seen_and_exhausts() {
        let mut fresh = Fresh::new(80, 0);
        assert_eq!(fresh.next(&[]), Some(5));
        assert_eq!(fresh.next(&[8]), Some(11));
        assert_eq!(fresh.next(&[8, 14]), Some(17));

        let mut fresh = Fresh::new(80, 2);
        let order: Vec<u16> = std::iter::from_fn(|| fresh.next(&[])).take(8).collect();
        assert_eq!(order, [11, 14, 17, 20, 23, 5, 8, 1]);

        let mut fresh = Fresh::new(4, 0);
        let all: Vec<u16> = std::iter::from_fn(|| fresh.next(&[])).collect();
        assert_eq!(all, [1, 2, 3]);

        let mut fresh = Fresh::new(40, 5);
        let all: Vec<u16> = std::iter::from_fn(|| fresh.next(&[])).collect();
        assert_eq!(all.len(), 39);
        assert!(all.iter().all(|col| (1..40).contains(col)));
        assert_eq!(all[0], 20);
    }

    // A clean or single-stall call touches at most three consecutive entries,
    // so the next call (start + START_STEP) must open on none of them, or a
    // reply orphaned at a confirm column passes that call as a false clean.
    // `Fresh` drops columns at or past `cols`, so the guarantee is checked at
    // every width from the narrowest that fits all of `PREFERRED` upwards.
    #[test]
    fn consecutive_starts_share_no_columns() {
        let window = |cols: u16, start: usize| -> Vec<u16> {
            let mut fresh = Fresh::new(cols, start);
            std::iter::from_fn(|| fresh.next(&[])).take(3).collect()
        };
        for cols in [24, 30, 40, 43, 44, 80, 200] {
            for start in 0..PREFERRED.len() {
                let this = window(cols, start);
                let next = window(cols, start + START_STEP);
                assert!(
                    this.iter().all(|col| !next.contains(col)),
                    "cols {cols} start {start}: {this:?} vs {next:?}"
                );
            }
        }
    }

    #[test]
    fn usage_probe_start_beyond_preferred_wraps() {
        let wrapped = PREFERRED[usize::MAX % PREFERRED.len()];
        for (start, col) in [(7, 5), (8, 8), (usize::MAX, wrapped)] {
            let mut term = Term::new(&[]);
            assert_eq!(term.resync(80, start), Resync::Synced { drained: 0 });
            assert_eq!(term.moves, [col], "start {start}");
        }
    }

    // The reply orphaned at the confirm column would pass a call opening on
    // that column as clean; start + START_STEP opens elsewhere and drains it.
    #[test]
    fn usage_probe_stall_at_first_confirm_is_unanswered_without_retry() {
        let mut term = Term::new(&[(0, 7)]);
        term.stall_queries = 1;
        term.stall_from_query = 1;
        assert_eq!(term.resync(80, 0), Resync::Unanswered);
        assert_eq!(term.tally(), (2, 1));
        assert_eq!(term.moves, [5, 8]);
        assert_eq!(term.late, [(8, ROW)]);
        assert!(term.queue.is_empty());

        term.deliver_late();
        term.moves.clear();
        assert_eq!(term.resync(80, START_STEP), Resync::Synced { drained: 1 });
        assert_eq!(term.tally(), (3, 1));
        assert_eq!(term.moves, [14, 17, 20]);
        assert!(term.queue.is_empty());
    }

    #[test]
    fn usage_probe_stall_at_second_confirm_is_unanswered() {
        let mut term = Term::new(&[(0, 7)]);
        term.stall_queries = 1;
        term.stall_from_query = 2;
        assert_eq!(term.resync(80, 0), Resync::Unanswered);
        assert_eq!(term.tally(), (3, 1));
        assert_eq!(term.moves, [5, 8, 11]);
        assert_eq!(term.late, [(11, ROW)]);
    }

    #[test]
    fn usage_probe_confirms_skip_columns_seen_in_stale_replies() {
        let mut term = Term::new(&[(8, 7)]);
        assert_eq!(term.resync(80, 0), Resync::Synced { drained: 1 });
        assert_eq!(term.tally(), (3, 1));
        assert_eq!(term.moves, [5, 11, 14]);
    }

    #[test]
    fn usage_probe_pop_error_mid_drain_stops_immediately() {
        let mut term = Term::new(&[(0, 7), (1, 7)]);
        term.stall_queries = 1;
        assert_eq!(term.resync(80, 0), Resync::Unanswered);
        assert_eq!(term.tally(), (1, 2));
    }

    // Reads the process-global counter; no production caller runs in the test
    // binary (no tty), so the sequence is ours alone.
    #[test]
    fn usage_probe_next_sentinel_start_steps_by_three_and_visits_every_start() {
        let starts: Vec<usize> = (0..PREFERRED.len())
            .map(|_| next_sentinel_start())
            .collect();
        for pair in starts.windows(2) {
            assert_eq!(
                (pair[1] + PREFERRED.len() - pair[0]) % PREFERRED.len(),
                START_STEP,
                "{starts:?}"
            );
        }
        let mut visited = starts.clone();
        visited.sort_unstable();
        visited.dedup();
        assert_eq!(visited.len(), PREFERRED.len(), "{starts:?}");
        assert!(starts.iter().all(|start| *start < PREFERRED.len()));
    }

    #[test]
    fn usage_probe_narrow_pane_drops_unfitting_preferred_columns() {
        let mut term = Term::new(&[(0, 7)]);
        assert_eq!(term.resync(10, 0), Resync::Synced { drained: 1 });
        assert_eq!(term.tally(), (3, 1));
        assert_eq!(term.moves, [5, 8, 1]);

        // At 24 columns every `PREFERRED` entry fits; one narrower drops 23.
        let mut term = Term::new(&[]);
        assert_eq!(term.resync(24, 6), Resync::Synced { drained: 0 });
        assert_eq!(term.moves, [23]);
        let mut term = Term::new(&[]);
        assert_eq!(term.resync(23, 6), Resync::Synced { drained: 0 });
        assert_eq!(term.moves, [5]);
    }

    #[test]
    fn usage_probe_disjointness_guarantee_boundary_is_twenty_four_columns() {
        let window = |cols: u16, start: usize| -> Vec<u16> {
            let mut fresh = Fresh::new(cols, start);
            std::iter::from_fn(|| fresh.next(&[])).take(3).collect()
        };
        let overlaps = |cols: u16| {
            (0..PREFERRED.len()).any(|start| {
                let this = window(cols, start);
                let next = window(cols, start + START_STEP);
                this.iter().any(|col| next.contains(col))
            })
        };
        assert!(!overlaps(24));
        assert!(overlaps(23), "a 23-column pane should already overlap");
        for cols in [MIN_COLS, 5, 6, 9, 10, 23] {
            let w = window(cols, 6);
            assert!(w.iter().all(|col| (1..cols).contains(col)), "{cols}: {w:?}");
            assert_eq!(
                w.len(),
                w.iter().collect::<std::collections::BTreeSet<_>>().len()
            );
        }
    }

    #[cfg(unix)]
    mod wrapper {
        use super::*;
        use std::rc::Rc;

        const SAVE: &[u8] = b"\x1b7";
        const RESTORE: &[u8] = b"\x1b8";

        #[derive(Default)]
        struct RecordingRaw {
            enabled: bool,
            enables: usize,
            disables: usize,
        }

        impl RawMode for RecordingRaw {
            fn is_enabled(&self) -> bool {
                self.enabled
            }

            fn enable(&mut self) -> io::Result<()> {
                self.enables += 1;
                self.enabled = true;
                Ok(())
            }

            fn disable(&mut self) -> io::Result<()> {
                self.disables += 1;
                self.enabled = false;
                Ok(())
            }
        }

        struct SharedWriter(Rc<RefCell<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        // The column the last `MoveToColumn` left the cursor at, so `Term`
        // answers queries the way a terminal would.
        fn last_column_move(bytes: &[u8]) -> Option<u16> {
            let text = String::from_utf8_lossy(bytes);
            let seq = text.rsplit('\x1b').next()?;
            let col: u16 = seq.strip_prefix('[')?.strip_suffix('G')?.parse().ok()?;
            Some(col - 1)
        }

        fn resync_on_term(
            term: &mut Term,
            raw: &mut RecordingRaw,
            cols: u16,
            start: usize,
        ) -> (Resync, Vec<u8>) {
            let bytes = Rc::new(RefCell::new(Vec::new()));
            let mut writer = SharedWriter(Rc::clone(&bytes));
            let term = RefCell::new(term);
            let outcome = resync_on(
                &mut writer,
                cols,
                start,
                raw,
                || {
                    let mut term = term.borrow_mut();
                    term.col = last_column_move(&bytes.borrow()).unwrap_or(0);
                    term.query()
                },
                || term.borrow_mut().pop(),
            );
            let bytes = bytes.borrow().clone();
            (outcome, bytes)
        }

        #[test]
        fn clean_queue_writes_save_sentinel_restore() {
            let mut term = Term::new(&[]);
            let mut raw = RecordingRaw::default();
            let (outcome, bytes) = resync_on_term(&mut term, &mut raw, 80, 0);
            assert_eq!(outcome, Resync::Synced { drained: 0 });
            assert_eq!(bytes, [SAVE, b"\x1b[6G", RESTORE].concat());
            assert_eq!(term.tally(), (1, 0));
        }

        #[test]
        fn position_is_restored_on_every_outcome() {
            let mut term = Term::new(&[]);
            term.honours_moves = false;
            let mut raw = RecordingRaw::default();
            let (outcome, bytes) = resync_on_term(&mut term, &mut raw, 80, 0);
            assert_eq!(outcome, Resync::Unanswered);
            assert!(
                bytes.starts_with(SAVE) && bytes.ends_with(RESTORE),
                "{bytes:?}"
            );

            let mut term = Term::new(&[(0, 7), (1, 7)]);
            let (outcome, bytes) = resync_on_term(&mut term, &mut raw, 4, 0);
            assert_eq!(outcome, Resync::GaveUp { drained: 2 });
            assert!(
                bytes.starts_with(SAVE) && bytes.ends_with(RESTORE),
                "{bytes:?}"
            );
        }

        #[test]
        fn raw_mode_is_toggled_only_when_it_was_off() {
            let mut term = Term::new(&[]);
            let mut raw = RecordingRaw::default();
            resync_on_term(&mut term, &mut raw, 80, 0);
            assert_eq!((raw.enables, raw.disables), (1, 1));
            assert!(!raw.enabled);

            let mut raw = RecordingRaw {
                enabled: true,
                ..RecordingRaw::default()
            };
            resync_on_term(&mut term, &mut raw, 80, 0);
            assert_eq!((raw.enables, raw.disables), (0, 0));
            assert!(raw.enabled);
        }

        #[test]
        fn enable_failure_skips_without_touching_the_terminal() {
            struct Refusing;

            impl RawMode for Refusing {
                fn is_enabled(&self) -> bool {
                    false
                }

                fn enable(&mut self) -> io::Result<()> {
                    Err(io::Error::from(io::ErrorKind::Unsupported))
                }

                fn disable(&mut self) -> io::Result<()> {
                    panic!("disable after a failed enable")
                }
            }

            let mut bytes = Vec::new();
            let outcome = resync_on(
                &mut bytes,
                80,
                0,
                &mut Refusing,
                || panic!("query"),
                || panic!("pop"),
            );
            assert_eq!(outcome, Resync::Skipped);
            assert!(bytes.is_empty());
        }

        #[test]
        fn usage_probe_skipped_narrow_pane_still_restores_and_untoggles_raw() {
            let mut term = Term::new(&[(0, 7)]);
            let mut raw = RecordingRaw::default();
            let (outcome, bytes) = resync_on_term(&mut term, &mut raw, MIN_COLS - 1, 0);
            assert_eq!(outcome, Resync::Skipped);
            assert_eq!(bytes, [SAVE, RESTORE].concat());
            assert_eq!((raw.enables, raw.disables), (1, 1));
            assert!(!raw.enabled);
            assert_eq!(term.tally(), (0, 0));
            assert_eq!(term.queue.len(), 1, "nothing consumed on Skipped");
        }

        #[test]
        fn gate_skips_unless_both_ttys_are_real_and_backoff_is_idle() {
            let term = Some(OsStr::new("xterm-256color"));
            assert!(!should_skip(true, true, term, false));
            assert!(should_skip(false, true, term, false));
            assert!(should_skip(true, false, term, false));
            assert!(should_skip(true, true, Some(OsStr::new("dumb")), false));
            assert!(should_skip(true, true, term, true));
        }
    }

    #[cfg(unix)]
    mod muted {
        use super::*;
        use serial_test::serial;

        // Holds the stdout lock for the whole capture: the harness reports
        // from the main thread through `io::stdout()`, and a report caught
        // mid-`write(1)` while fd 1 is re-pointed fails with EPIPE and kills the run.
        struct SavedStdout {
            fd: OwnedFd,
            _lock: io::StdoutLock<'static>,
        }

        impl Drop for SavedStdout {
            fn drop(&mut self) {
                // SAFETY: both descriptors are open; dup2 takes no pointers.
                assert_ne!(
                    unsafe { libc::dup2(self.fd.as_raw_fd(), libc::STDOUT_FILENO) },
                    -1
                );
            }
        }

        // Points fd 1 at a fresh pipe; the reader sees EOF once the guard
        // restores fd 1, because fd 1 is the pipe's only writer. The read end
        // is non-blocking so a writer inherited by a child of another test
        // fails `read_all` at its deadline instead of hanging the test.
        fn capture_stdout() -> (SavedStdout, File) {
            let mut lock = io::stdout().lock();
            lock.flush().unwrap();
            // SAFETY: fcntl with F_DUPFD_CLOEXEC takes no pointers; the result is checked.
            let saved = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
            assert_ne!(saved, -1);
            // SAFETY: `saved` was just created by fcntl and nothing else owns it.
            let saved = SavedStdout {
                fd: unsafe { OwnedFd::from_raw_fd(saved) },
                _lock: lock,
            };

            let mut fds = [0; 2];
            // SAFETY: `fds` is a valid two-element array for pipe to fill.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: fcntl with F_GETFL/F_SETFL takes no pointers; results are checked.
            let flags = unsafe { libc::fcntl(fds[0], libc::F_GETFL) };
            assert_ne!(flags, -1);
            // SAFETY: as above.
            let rc = unsafe { libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK) };
            assert_ne!(rc, -1);
            // SAFETY: both ends were just created by pipe and nothing else owns them.
            let (reader, writer) =
                unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
            // SAFETY: both descriptors are open; dup2 takes no pointers.
            assert_ne!(
                unsafe { libc::dup2(writer.as_raw_fd(), libc::STDOUT_FILENO) },
                -1
            );
            drop(writer);
            (saved, reader)
        }

        fn write_fd1(bytes: &[u8]) {
            // SAFETY: `bytes` is a valid buffer of the given length.
            let n = unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
            assert_eq!(n, bytes.len() as isize);
        }

        fn read_all(reader: &mut File) -> String {
            use std::io::Read;
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut out = Vec::new();
            let mut buf = [0; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "pipe still open: {out:?}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => panic!("{err}"),
                }
            }
            String::from_utf8(out).unwrap()
        }

        // The harness reports from other threads on fd 1 too, so the capture
        // is checked for our markers and then passed through to the real fd 1.
        fn assert_only_kept_marker_landed(saved: SavedStdout, mut reader: File) {
            drop(saved);
            let out = read_all(&mut reader);
            assert!(!out.contains("<gone>"), "{out:?}");
            assert!(out.contains("<kept>"), "{out:?}");
            write_fd1(out.replacen("<kept>", "", 1).as_bytes());
        }

        #[test]
        #[serial]
        fn writes_under_the_guard_vanish_and_fd_one_is_restored() {
            let (saved, reader) = capture_stdout();
            {
                let muted = mute_stdout().unwrap();
                // SAFETY: fcntl with F_GETFD takes no pointers.
                let flags = unsafe { libc::fcntl(muted.saved.as_raw_fd(), libc::F_GETFD) };
                assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
                write_fd1(b"<gone>");
            }
            write_fd1(b"<kept>");
            assert_only_kept_marker_landed(saved, reader);
        }

        #[test]
        #[serial]
        fn fd_one_is_restored_after_a_panic_under_the_guard() {
            let (saved, reader) = capture_stdout();
            let result = std::panic::catch_unwind(|| {
                let _muted = mute_stdout().unwrap();
                panic!("inside the guard");
            });
            assert!(result.is_err());
            write_fd1(b"<kept>");
            assert_only_kept_marker_landed(saved, reader);
        }
    }

    #[cfg(unix)]
    #[test]
    fn backoff_doubles_to_cap() {
        assert_eq!(next_backoff(None), Duration::from_secs(30));
        assert_eq!(
            next_backoff(Some(Duration::from_secs(30))),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(240))),
            Duration::from_secs(300)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(300))),
            Duration::from_secs(300)
        );
    }

    #[cfg(unix)]
    fn unanswered(backoff: &mut Backoff, times: u32, now: Instant) {
        for _ in 0..times {
            backoff.record(Resync::Unanswered, now);
        }
    }

    #[cfg(unix)]
    #[test]
    fn two_unanswered_do_not_arm() {
        let now = Instant::now();
        let mut backoff = Backoff::new();
        unanswered(&mut backoff, 2, now);
        assert_eq!(backoff.unanswered, 2);
        assert!(!backoff.armed(now));
    }

    #[cfg(unix)]
    #[test]
    fn three_unanswered_arm_for_thirty_seconds() {
        let now = Instant::now();
        let mut backoff = Backoff::new();
        unanswered(&mut backoff, 3, now);
        assert_eq!(backoff.armed, Some((now + BACKOFF_FIRST, BACKOFF_FIRST)));
        assert!(backoff.armed(now));
        assert!(backoff.armed(now + Duration::from_secs(29)));
        assert!(!backoff.armed(now + BACKOFF_FIRST));
    }

    #[cfg(unix)]
    #[test]
    fn synced_resets_streak() {
        let now = Instant::now();
        let mut backoff = Backoff::new();
        unanswered(&mut backoff, 2, now);
        backoff.record(Resync::Synced { drained: 1 }, now);
        assert_eq!(backoff.unanswered, 0);
        unanswered(&mut backoff, 1, now);
        assert_eq!(backoff.unanswered, 1);
        assert!(!backoff.armed(now));
    }

    #[cfg(unix)]
    #[test]
    fn unanswered_after_window_rearms_doubled() {
        let now = Instant::now();
        let mut backoff = Backoff::new();
        unanswered(&mut backoff, 3, now);
        let later = now + BACKOFF_FIRST + Duration::from_secs(1);
        assert!(!backoff.armed(later));
        backoff.record(Resync::Unanswered, later);
        let doubled = Duration::from_secs(60);
        assert_eq!(backoff.armed, Some((later + doubled, doubled)));
        assert!(backoff.armed(later));
    }

    #[cfg(unix)]
    #[test]
    fn synced_while_armed_disarms() {
        let now = Instant::now();
        let mut backoff = Backoff::new();
        unanswered(&mut backoff, 3, now);
        assert!(backoff.armed(now));
        backoff.record(Resync::Synced { drained: 0 }, now);
        assert!(!backoff.armed(now));
        assert_eq!(backoff.unanswered, 0);
        assert_eq!(backoff.armed, None);
    }

    #[cfg(unix)]
    #[test]
    fn gave_up_leaves_streak_untouched() {
        let now = Instant::now();
        let mut backoff = Backoff::new();
        unanswered(&mut backoff, 2, now);
        backoff.record(Resync::GaveUp { drained: MAX_POPS }, now);
        backoff.record(Resync::Skipped, now);
        assert_eq!(backoff.unanswered, 2);
        assert!(!backoff.armed(now));
        assert_eq!(backoff.armed, None);
    }

    #[cfg(unix)]
    #[test]
    fn dumb_terminals_are_detected() {
        assert!(term_is_dumb(None));
        assert!(term_is_dumb(Some(OsStr::new(""))));
        assert!(term_is_dumb(Some(OsStr::new("dumb"))));
        assert!(!term_is_dumb(Some(OsStr::new("xterm-256color"))));
    }
}
