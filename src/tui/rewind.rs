use super::App;
use super::bottom_pane::{RewindState, RewindTarget, View};
use super::cells::{Cell, RewindBarrier};
use super::sink::UiMsg;
use crate::config::RequestContext;
use crate::events::NoticeKind;
use std::time::Duration;

/// How close two idle Escs must come to open the rewind picker.
pub(super) const REWIND_ESC_GAP: Duration = Duration::from_secs(1);

/// The command word of a submitted line under the active prefix, in its
/// dot form.
fn command_word(line: &str, prefix: crate::config::CommandPrefix) -> Option<String> {
    prefix
        .normalize(line.trim())
        .and_then(|command| command.split_whitespace().next().map(str::to_string))
}

/// Whether a submitted command line is `.undo` under the active prefix.
pub(super) fn is_undo(line: &str, prefix: crate::config::CommandPrefix) -> bool {
    command_word(line, prefix).as_deref() == Some(".undo")
}

/// Commands whose reply rewrites the exchange they follow instead of
/// opening one: `Session::add_message` appends to the last assistant
/// message for `.continue` and replaces it for `.regenerate`, so the
/// exchange `.undo` removes after either is the one before the command.
/// Every other command whose request reached the model (`.file`, `.run`,
/// `.starter`, `.prompt`, a macro, ...) recorded a fresh exchange the
/// way a prompt does.
const EXCHANGE_MODIFYING_COMMANDS: [&str; 2] = [".continue", ".regenerate"];

/// Whether the cell opened an exchange `.undo` would take back: a user
/// line or a command turn whose request reached the model, the command
/// not in `EXCHANGE_MODIFYING_COMMANDS`. A request that failed or was
/// interrupted before any reply still counts: the session keeps the user
/// message with an interrupted-response marker. A turn that failed
/// before the request (no API key, a usage bail on bare `.file`) or
/// never asks the model (`.info`) recorded nothing, so it is looked past.
pub(super) fn opens_exchange(cell: &Cell, prefix: crate::config::CommandPrefix) -> bool {
    match cell {
        Cell::User { opened, .. } => *opened,
        Cell::Command { name, opened, .. } => {
            *opened
                && command_word(name, prefix)
                    .is_some_and(|word| !EXCHANGE_MODIFYING_COMMANDS.contains(&word.as_str()))
        }
        _ => false,
    }
}

/// Whether one `.undo` takes the session back to just before the cell:
/// an exchange's opener, or a line sent mid-turn, which the session undoes
/// on its own ahead of the turn it rode in on.
fn starts_undo_step(cell: &Cell, prefix: crate::config::CommandPrefix) -> bool {
    undo_step_text(cell, prefix).is_some()
}

/// What the composer gets back when the transcript is taken back to the
/// cell: the line typed, or the command; None for a cell that starts no
/// undo step.
fn undo_step_text(cell: &Cell, prefix: crate::config::CommandPrefix) -> Option<&str> {
    let starts = opens_exchange(cell, prefix) || matches!(cell, Cell::User { mid_turn: true, .. });
    match cell {
        Cell::User { text, .. } if starts => Some(text),
        Cell::Command { name, .. } if starts => Some(name),
        _ => None,
    }
}

/// Index of the cell a `.undo` at `undo` takes the transcript back to:
/// the latest opener or mid-turn line before it, scanning back past any
/// reply.
pub(super) fn undo_opener(
    cells: &[Cell],
    undo: usize,
    prefix: crate::config::CommandPrefix,
) -> Option<usize> {
    cells[..undo]
        .iter()
        .rposition(|cell| starts_undo_step(cell, prefix))
}

/// The cells each `.undo` from the end would take the transcript back to,
/// newest first, down to `oldest`.
fn undo_steps_from(
    cells: &[Cell],
    oldest: usize,
    prefix: crate::config::CommandPrefix,
) -> Vec<usize> {
    (oldest..cells.len())
        .rev()
        .filter(|&index| starts_undo_step(&cells[index], prefix))
        .collect()
}

/// What stops the rewind picker at a command line, under the macros
/// enabled when it ran: a macro (`.<name>` or `.macro <name>`, not the
/// enable/disable toggles) stands for exchanges the transcript never
/// counted; `.session`, `.agent <name>`, `.fork`, `.undo`, `.edit
/// session` and the session and agent forms of `.exit`, `.empty` and
/// `.compress` leave the context holding history the transcript never
/// showed. A bare `.agent` only prints its usage, and `.macro <name>`
/// for a name the list had not got, with the model never asked, created
/// the macro in the editor rather than running one.
pub(super) fn rewind_barrier(
    line: &str,
    prefix: crate::config::CommandPrefix,
    macros: &[(String, String)],
    asked_model: bool,
) -> Option<RewindBarrier> {
    let line = prefix.normalize(line.trim())?;
    let mut words = line.split_whitespace();
    let known = |word: &str| macros.iter().any(|(name, _)| name == word);
    match (words.next()?, words.next()) {
        (".macro", Some(first)) => (!matches!(first, "enable" | "disable")
            && (asked_model || known(first)))
        .then_some(RewindBarrier::Macro),
        (".session" | ".fork" | ".undo", _) | (".agent", Some(_)) => Some(RewindBarrier::Session),
        (".exit", Some("session" | "agent"))
        | (".empty" | ".compress" | ".edit", Some("session")) => Some(RewindBarrier::Session),
        (word, _) => word
            .strip_prefix('.')
            .filter(|word| known(word))
            .map(|_| RewindBarrier::Macro),
    }
}

/// Takes the `pops` newest exchanges out of the session, stopping at
/// the first that cannot be undone: how many came out, and what stopped
/// it short.
pub(super) fn rewind_exchanges(
    ctx: &mut RequestContext,
    pops: usize,
) -> (usize, Option<anyhow::Error>) {
    for popped in 0..pops {
        if let Err(err) = ctx.undo_last_exchange() {
            return (popped, Some(err));
        }
    }
    (pops, None)
}

/// Takes the chosen exchanges out of the session and the transcript and
/// gives the oldest one's prompt back to the composer. Every offered row
/// sits above the newest macro turn, so each one is a single exchange;
/// a line sent mid-turn is one more `.undo` for the session, so popping
/// one per opener and one per such line keeps the session and the
/// transcript in step. The transcript goes back only as far as the session did, and a
/// stop short of the choice is said. Without a session there is nothing
/// to pop and the transcript is the whole of the rewind. The composer
/// gets the row's own text, not the session's prefill: a command row
/// holds the command, which the session never did.
pub(super) fn apply_rewind(app: &mut App, ctx: &mut RequestContext, target: RewindTarget) {
    let Some(oldest) = target.rows.last() else {
        return;
    };
    let prefix = app.config.command_prefix;
    let steps = undo_steps_from(app.transcript.cells(), oldest.opener, prefix);
    let (cut, stopped) = if ctx.session.is_some() {
        rewind_exchanges(ctx, steps.len())
    } else {
        (steps.len(), None)
    };
    app.sync_from_ctx(ctx);
    if let Some(&at) = cut.checked_sub(1).and_then(|last| steps.get(last)) {
        let title = undo_step_text(&app.transcript.cells()[at], prefix)
            .unwrap_or_default()
            .to_string();
        app.transcript.truncate(at);
        app.set_composer_text(&title);
        app.pasted_images.clear();
    }
    app.refresh_palette(Some(ctx));
    // The notice is a transcript cell, so it goes in after the cut or
    // the cut would take it with the exchanges.
    if let Some(err) = stopped {
        app.handle_msg(UiMsg::Notice {
            kind: NoticeKind::ToolWarning,
            text: format!("Rewind stopped: {err}"),
        });
    }
}

impl App {
    /// A second idle Esc within the gap opens the rewind picker, when
    /// the transcript holds an exchange to go back to.
    pub(super) fn idle_esc(&mut self) {
        let now = self.clock.now();
        let second = self
            .last_esc
            .is_some_and(|at| now.duration_since(at) <= REWIND_ESC_GAP);
        self.last_esc = (!second).then_some(now);
        if !second {
            return;
        }
        let (rewind, barrier) =
            RewindState::from_cells(self.transcript.cells(), self.config.command_prefix);
        if let Some(barrier) = barrier {
            self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                format!("Rewind stops at the last {}", barrier.kind_name()),
            );
        }
        if let Some(rewind) = rewind {
            self.view = View::Rewind(rewind);
        }
    }
}
