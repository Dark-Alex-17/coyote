use crate::config::{RequestContext, SessionInstance};
use unicode_width::UnicodeWidthStr;

pub(super) use crate::config::SAVED_SESSION_LINE;

/// The names of what is active, compared across returns of the context
/// to tell what was entered and what was left.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CtxSnapshot {
    pub(super) agent: Option<String>,
    pub(super) role: Option<String>,
    pub(super) session: Option<String>,
    /// The live session the name belongs to, so a save under a new name
    /// reads as a rename rather than a leave.
    pub(super) session_instance: Option<SessionInstance>,
    pub(super) rag: Option<String>,
}

impl CtxSnapshot {
    pub(super) fn from_ctx(ctx: &RequestContext) -> Self {
        Self {
            agent: ctx.agent.as_ref().map(|agent| agent.name().to_string()),
            role: ctx.role.as_ref().map(|role| role.name().to_string()),
            session: ctx
                .session
                .as_ref()
                .map(|session| session.name().to_string()),
            session_instance: ctx.session.as_ref().map(|session| session.instance()),
            rag: ctx.rag.as_ref().map(|rag| rag.name().to_string()),
        }
    }

    fn named(&self) -> [(&'static str, Option<&str>); 4] {
        [
            ("agent", self.agent.as_deref()),
            ("role", self.role.as_deref()),
            ("session", self.session.as_deref()),
            ("rag", self.rag.as_deref()),
        ]
    }

    /// Whether the session here goes on in `new` under another name, as
    /// `.save session <name>` leaves it.
    pub(super) fn renames_session_in(&self, new: &CtxSnapshot) -> bool {
        self.session.is_some()
            && new.session.is_some()
            && self.session != new.session
            && self.session_instance.is_some()
            && self.session_instance == new.session_instance
    }

    /// Whether anything active here is not in `new`; a renamed session
    /// is still active.
    pub(super) fn leaves_for(&self, new: &CtxSnapshot) -> bool {
        let renamed = self.renames_session_in(new);
        self.named()
            .into_iter()
            .zip(new.named())
            .any(|((kind, was), (_, now))| {
                was.is_some() && was != now && !(renamed && kind == "session")
            })
    }
}

/// What a divider between `old` and `new` says: everything left, then
/// everything entered, each as `<kind> <name>` joined by ` . `. A left
/// session reads `saved` when `saved` is set; a renamed one reads
/// `session <old> saved as <new>`. None when nothing changed.
pub(super) fn divider_text(old: &CtxSnapshot, new: &CtxSnapshot, saved: bool) -> Option<String> {
    let mut left = Vec::new();
    let mut entered = Vec::new();
    let renamed = old.renames_session_in(new);
    for ((kind, was), (_, now)) in old.named().into_iter().zip(new.named()) {
        if was == now || (renamed && kind == "session") {
            continue;
        }
        if let Some(name) = was {
            let mark = if saved && kind == "session" {
                " saved"
            } else {
                ""
            };
            left.push(format!("{kind} {name}{mark}"));
        }
        if let Some(name) = now {
            entered.push(format!("{kind} {name}"));
        }
    }
    let mut groups: Vec<String> = [("left", left), ("entered", entered)]
        .into_iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(verb, items)| format!("{verb} {}", items.join(" . ")))
        .collect();
    if let (true, Some(was), Some(now)) = (renamed, &old.session, &new.session) {
        groups.push(format!("session {was} saved as {now}"));
    }
    (!groups.is_empty()).then(|| groups.join(" . "))
}

/// `output` without its lines that hold `needle`; None when none does.
pub(super) fn without_lines_holding(output: &str, needle: &str) -> Option<String> {
    if !output.contains(needle) {
        return None;
    }
    let kept: String = output
        .split_inclusive('\n')
        .filter(|line| !line.contains(needle))
        .collect();
    Some(kept)
}

/// The context label as a top rule's title, padded by a space each side,
/// in what a rule `width` wide leaves once `taken` titles have their
/// width and the one space ratatui puts after each. A label too wide
/// loses its start behind `...`; None when not even that fits.
pub(super) fn context_title(label: &str, width: u16, taken: &[&str]) -> Option<String> {
    let room = taken.iter().fold(usize::from(width), |room, title| {
        room.saturating_sub(title.width() + 1)
    });
    let padded = format!(" {label} ");
    if padded.width() <= room {
        return Some(padded);
    }
    let budget = room.checked_sub(" ... ".len())?;
    if budget == 0 {
        return None;
    }
    let mut tail = String::new();
    let mut used = 0;
    for c in label.chars().rev() {
        let w = c.to_string().width();
        if used + w > budget {
            break;
        }
        used += w;
        tail.insert(0, c);
    }
    Some(format!(" ...{tail} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(agent: Option<&str>, role: Option<&str>, session: Option<&str>) -> CtxSnapshot {
        CtxSnapshot {
            agent: agent.map(str::to_string),
            role: role.map(str::to_string),
            session: session.map(str::to_string),
            session_instance: None,
            rag: None,
        }
    }

    #[test]
    fn an_unchanged_context_has_no_divider() {
        let same = snapshot(Some("architect"), None, Some("coyote-tui"));
        assert_eq!(divider_text(&same, &same, true), None);
        assert!(!same.leaves_for(&same));
    }

    #[test]
    fn entering_names_only_what_is_new() {
        let old = snapshot(None, None, None);
        let new = snapshot(Some("sisyphus"), None, None);
        assert_eq!(
            divider_text(&old, &new, false).as_deref(),
            Some("entered agent sisyphus")
        );
        assert!(!old.leaves_for(&new));
        assert!(new.leaves_for(&old));
    }

    #[test]
    fn leaving_names_everything_left_and_marks_a_saved_session() {
        let old = snapshot(Some("architect"), None, Some("coyote-tui"));
        let new = CtxSnapshot::default();
        assert_eq!(
            divider_text(&old, &new, true).as_deref(),
            Some("left agent architect . session coyote-tui saved")
        );
        assert_eq!(
            divider_text(&old, &new, false).as_deref(),
            Some("left agent architect . session coyote-tui")
        );
    }

    #[test]
    fn a_switch_says_what_was_left_before_what_was_entered() {
        let old = snapshot(None, Some("coder"), Some("one"));
        let new = CtxSnapshot {
            rag: Some("docs".to_string()),
            ..snapshot(None, Some("reviewer"), Some("two"))
        };
        assert_eq!(
            divider_text(&old, &new, false).as_deref(),
            Some("left role coder . session one . entered role reviewer . session two . rag docs")
        );
    }

    /// The same live session under a new name is a rename: no leave, and
    /// the divider says what it was saved as; another session under the
    /// new name is a leave as before.
    #[test]
    fn a_session_saved_under_a_new_name_is_renamed_not_left() {
        let instance = SessionInstance::default();
        let named = |name: &str, instance| CtxSnapshot {
            session_instance: Some(instance),
            ..snapshot(Some("architect"), None, Some(name))
        };
        let old = named("temp", instance);
        let renamed = named("notes", instance);
        assert!(old.renames_session_in(&renamed));
        assert!(!old.leaves_for(&renamed));
        assert_eq!(
            divider_text(&old, &renamed, true).as_deref(),
            Some("session temp saved as notes")
        );
        let other = named("notes", SessionInstance::default());
        assert!(!old.renames_session_in(&other));
        assert!(old.leaves_for(&other));
        assert_eq!(
            divider_text(&old, &other, false).as_deref(),
            Some("left session temp . entered session notes")
        );
    }

    #[test]
    fn only_the_lines_holding_the_needle_go() {
        let output = "\u{2713} Saved the session to '/tmp/a.yaml'.\nbye\n";
        assert_eq!(
            without_lines_holding(output, SAVED_SESSION_LINE).as_deref(),
            Some("bye\n")
        );
        assert_eq!(without_lines_holding("bye\n", SAVED_SESSION_LINE), None);
    }

    #[test]
    fn the_title_pads_the_label_when_it_fits() {
        assert_eq!(
            context_title("architect > coyote-tui", 80, &[" your turn "]).as_deref(),
            Some(" architect > coyote-tui ")
        );
    }

    #[test]
    fn a_title_too_wide_loses_its_start_and_never_reaches_the_other_titles() {
        let taken = [" NORMAL ", " your turn "];
        let title = context_title("architect > coyote-tui", 30, &taken).unwrap();
        assert_eq!(title, " ...-tui ");
        let used: usize = taken.iter().map(|t| t.width() + 1).sum();
        assert!(title.width() + used <= 30, "{title:?}");
        assert_eq!(context_title("architect", 24, &taken), None);
    }
}
