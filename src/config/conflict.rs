//! Shared keep/replace conflict resolution for installers that must not
//! silently clobber locally modified files. The bundle installer and the
//! builtin hook installers all funnel per-file conflicts through
//! [`resolve_conflict`], so the prompt wording and the sticky
//! keep-all/replace-all semantics are identical everywhere, including when
//! one sticky scope spans several install locations in a single run.

use crate::prompt::{Prompter, select_closed};
use crate::utils::IS_STDOUT_TERMINAL;

use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallMode {
    Skip,
    Force,
    Prompt,
}

/// A `keep-all`/`replace-all` answer outlives the file it was given for:
/// the caller threads one value through every subsequent conflict in the
/// same run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StickyMode {
    None,
    KeepAll,
    ReplaceAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictAction {
    Keep,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NonInteractive {
    Bail,
    Keep,
}

pub(crate) fn should_replace_existing(
    dst: &Path,
    content: &str,
    category: &str,
    mode: InstallMode,
    sticky: &mut StickyMode,
    prompter: &dyn Prompter,
) -> Result<bool> {
    match mode {
        InstallMode::Skip => Ok(false),
        InstallMode::Force => Ok(true),
        InstallMode::Prompt => {
            if fs::read(dst).is_ok_and(|local| local == content.as_bytes()) {
                return Ok(false);
            }
            match resolve_conflict(dst, category, sticky, NonInteractive::Keep, prompter)? {
                ConflictAction::Keep => Ok(false),
                ConflictAction::Replace => Ok(true),
            }
        }
    }
}

pub(crate) fn resolve_conflict(
    dst: &Path,
    category: &str,
    sticky: &mut StickyMode,
    non_interactive: NonInteractive,
    prompter: &dyn Prompter,
) -> Result<ConflictAction> {
    match *sticky {
        StickyMode::KeepAll => return Ok(ConflictAction::Keep),
        StickyMode::ReplaceAll => return Ok(ConflictAction::Replace),
        StickyMode::None => {}
    }

    if !menu_can_use_terminal() {
        match non_interactive {
            NonInteractive::Keep => return Ok(ConflictAction::Keep),
            NonInteractive::Bail => {
                return Err(refuse_non_interactive(
                    format_args!("overwrite local file {}", dst.display()),
                    ", with --install-force (installs), or with --yes (updates).",
                ));
            }
        }
    }

    let prompt = format!("Conflict at {} (category: {category})", dst.display());
    let choice = select_choice(&prompt, prompter)?;

    match choice.as_str() {
        "keep" => Ok(ConflictAction::Keep),
        "replace" => Ok(ConflictAction::Replace),
        "keep-all" => {
            *sticky = StickyMode::KeepAll;
            Ok(ConflictAction::Keep)
        }
        "replace-all" => {
            *sticky = StickyMode::ReplaceAll;
            Ok(ConflictAction::Replace)
        }
        "abort" => bail!("Install aborted by user at conflict resolution."),
        _ => unreachable!("select_closed only hands back an offered option"),
    }
}

/// Whether a question has somewhere to be shown: the full-screen UI's
/// modal, or a terminal on stdout for an inquire widget.
pub(crate) fn menu_can_use_terminal() -> bool {
    #[cfg(test)]
    let on_tty = prompt_script::forced_tty().unwrap_or(*IS_STDOUT_TERMINAL);
    #[cfg(not(test))]
    let on_tty = *IS_STDOUT_TERMINAL;
    crate::tui::active() || on_tty
}

/// Where a refused menu can be answered instead: the plain REPL when the
/// full-screen UI is what stands in the way, a terminal otherwise.
pub(crate) fn rerun_hint() -> &'static str {
    if crate::tui::active() {
        "Re-run in the plain REPL"
    } else {
        "Re-run in a terminal"
    }
}

/// The refusal for a question nothing can show; every site builds it
/// here so the message has one shape. `remedy` follows the re-run hint
/// directly, so it begins with its own separator (" or pass --force.",
/// ", with --yes (updates).") or is just a period.
pub(crate) fn refuse_non_interactive(
    action: impl std::fmt::Display,
    remedy: impl std::fmt::Display,
) -> anyhow::Error {
    anyhow!(
        "Refusing to {action} non-interactively. {}{remedy}",
        rerun_hint()
    )
}

fn select_choice(prompt: &str, prompter: &dyn Prompter) -> Result<String> {
    select_closed(
        prompter,
        prompt,
        ["keep", "replace", "keep-all", "replace-all", "abort"]
            .map(str::to_string)
            .to_vec(),
    )
    .with_context(|| "failed to read conflict choice")
}

#[cfg(test)]
pub(crate) mod prompt_script {
    //! Scripted stand-in for the conflict menu. While a guard is installed
    //! the terminal check takes its answer from the guard, and the
    //! [`Scripted`] prompter serves each select from a queue of scripted
    //! answers, counting every prompt asked. A forced-terminal guard with
    //! an empty queue pins the "never prompts" invariant: any prompt panics
    //! and the counter stays 0.

    use crate::prompt::{PromptAnswer, PromptRequest, Prompter};
    use anyhow::Result;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const INACTIVE: usize = 0;
    const TTY: usize = 1;
    const NON_TTY: usize = 2;

    static STATE: AtomicUsize = AtomicUsize::new(INACTIVE);
    static ASKED: AtomicUsize = AtomicUsize::new(0);
    // A test that panics while holding the queue poisons its mutex; every
    // lock site recovers via `into_inner` so later tests keep working.
    static ANSWERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Installs the script with a forced terminal until the guard drops.
    /// The script is process-global, so tests using it must serialize
    /// (`#[serial]`). Answers are consumed front to back; a prompt beyond
    /// the scripted answers panics.
    #[must_use]
    pub fn install(answers: &[&str]) -> ScriptGuard {
        install_with_state(TTY, answers)
    }

    /// Installs the script with the terminal forced absent, for pinning the
    /// non-interactive behavior regardless of where the tests actually run.
    #[must_use]
    pub fn install_non_interactive() -> ScriptGuard {
        install_with_state(NON_TTY, &[])
    }

    fn install_with_state(state: usize, answers: &[&str]) -> ScriptGuard {
        *ANSWERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            answers.iter().map(|answer| answer.to_string()).collect();
        ASKED.store(0, Ordering::SeqCst);
        STATE.store(state, Ordering::SeqCst);
        ScriptGuard
    }

    pub struct ScriptGuard;

    impl Drop for ScriptGuard {
        fn drop(&mut self) {
            STATE.store(INACTIVE, Ordering::SeqCst);
            ANSWERS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
    }

    /// Number of prompts asked since the script was installed.
    pub fn prompts_asked() -> usize {
        ASKED.load(Ordering::SeqCst)
    }

    pub(super) fn forced_tty() -> Option<bool> {
        match STATE.load(Ordering::SeqCst) {
            TTY => Some(true),
            NON_TTY => Some(false),
            _ => None,
        }
    }

    fn next_answer(prompt: &str) -> String {
        ASKED.fetch_add(1, Ordering::SeqCst);
        let mut answers = ANSWERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            !answers.is_empty(),
            "conflict prompt asked with no scripted answer left: {prompt}"
        );
        answers.remove(0)
    }

    /// The prompter that answers every select from the installed script.
    #[derive(Debug)]
    pub struct Scripted;

    impl Prompter for Scripted {
        fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer> {
            Ok(PromptAnswer::Selected(next_answer(&req.question)))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::{
        HeadlessPrompter, NeedsHuman, PromptAction, PromptAnswer, ScriptedPrompter,
    };
    use prompt_script::Scripted;
    use serial_test::serial;
    use std::path::PathBuf;

    fn dst() -> PathBuf {
        PathBuf::from("/tmp/coyote-conflict-tests/example.yaml")
    }

    fn resolve(
        category: &str,
        sticky: &mut StickyMode,
        non_interactive: NonInteractive,
    ) -> Result<ConflictAction> {
        resolve_conflict(&dst(), category, sticky, non_interactive, &Scripted)
    }

    #[test]
    fn sticky_modes_short_circuit_without_prompting() {
        let mut sticky = StickyMode::KeepAll;
        assert!(matches!(
            resolve("macros", &mut sticky, NonInteractive::Bail).unwrap(),
            ConflictAction::Keep
        ));

        let mut sticky = StickyMode::ReplaceAll;
        assert!(matches!(
            resolve("macros", &mut sticky, NonInteractive::Bail).unwrap(),
            ConflictAction::Replace
        ));
    }

    /// Under the full-screen UI the conflict menu is asked through the
    /// prompter as a closed select, question and options intact.
    #[test]
    #[serial]
    fn select_choice_under_the_tui_asks_through_the_prompter() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Selected("replace".into())]);

        let choice = select_choice("Conflict on example.yaml", &prompter).unwrap();

        assert_eq!(choice, "replace");
        let requests = prompter.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].action, PromptAction::Select);
        assert_eq!(requests[0].question, "Conflict on example.yaml");
        assert!(requests[0].closed);
        assert_eq!(
            requests[0].options,
            ["keep", "replace", "keep-all", "replace-all", "abort"]
        );
    }

    /// A run with no human refuses the conflict menu with the typed
    /// select refusal and draws nothing on the terminal.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn headless_select_choice_is_a_typed_select_refusal_with_nothing_on_stdout() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let (saved, mut reader) = crate::testing::capture_stdout();

        let err = select_choice("Conflict on example.yaml", &HeadlessPrompter).unwrap_err();

        drop(saved);
        let out = crate::testing::read_all(&mut reader);
        assert!(out.is_empty(), "{out:?}");
        assert!(
            matches!(
                err.root_cause().downcast_ref::<NeedsHuman>(),
                Some(NeedsHuman::Prompt(PromptAction::Select))
            ),
            "{err:#}"
        );
    }

    #[test]
    #[serial]
    fn an_answer_outside_the_offered_options_is_an_error() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Selected("maybe".into())]);
        let mut sticky = StickyMode::None;

        let err = resolve_conflict(
            &dst(),
            "hooks",
            &mut sticky,
            NonInteractive::Keep,
            &prompter,
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("'maybe' is not one of the offered options"),
            "got: {err:#}"
        );
        assert_eq!(sticky, StickyMode::None);
    }

    #[test]
    #[serial]
    fn menu_can_use_terminal_is_true_under_the_tui_whatever_the_tty() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        assert!(menu_can_use_terminal());

        let _tui_off = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, false);
        assert_eq!(menu_can_use_terminal(), *IS_STDOUT_TERMINAL);
    }

    #[test]
    #[serial]
    fn the_refusal_names_the_action_and_where_to_re_run() {
        let _tui_off = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, false);
        let err = refuse_non_interactive("overwrite local file", " or pass --force.");
        assert_eq!(
            err.to_string(),
            "Refusing to overwrite local file non-interactively. Re-run in a terminal or pass --force."
        );

        let _tui_on = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let err = refuse_non_interactive("pick a bundle", ".");
        assert_eq!(
            err.to_string(),
            "Refusing to pick a bundle non-interactively. Re-run in the plain REPL."
        );
    }

    #[test]
    #[serial]
    fn non_interactive_bail_keeps_the_bundle_error_shape() {
        let _script = prompt_script::install_non_interactive();
        let mut sticky = StickyMode::None;

        let err = resolve("macros", &mut sticky, NonInteractive::Bail).unwrap_err();

        assert!(
            err.to_string().contains("Refusing to overwrite local file"),
            "got: {err}"
        );
        assert!(
            err.to_string()
                .contains("non-interactively. Re-run in a terminal"),
            "got: {err}"
        );
        assert!(err.to_string().contains("example.yaml"), "got: {err}");
        assert_eq!(prompt_script::prompts_asked(), 0);
    }

    #[test]
    #[serial]
    fn non_interactive_keep_resolves_without_error_or_prompt() {
        let _script = prompt_script::install_non_interactive();
        let mut sticky = StickyMode::None;

        let action = resolve("hooks", &mut sticky, NonInteractive::Keep).unwrap();

        assert!(matches!(action, ConflictAction::Keep));
        assert_eq!(sticky, StickyMode::None);
        assert_eq!(prompt_script::prompts_asked(), 0);
    }

    /// The full-screen UI has a modal to show the menu in, so a forced
    /// non-tty does not make either caller take its non-interactive
    /// answer: both ask and take what the prompter says.
    #[test]
    #[serial]
    fn under_the_tui_a_forced_non_tty_still_asks_through_the_prompter() {
        let _script = prompt_script::install_non_interactive();
        let _tui_on = crate::testing::AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let mut sticky = StickyMode::None;

        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Selected("replace".into())]);
        let action = resolve_conflict(
            &dst(),
            "hooks",
            &mut sticky,
            NonInteractive::Keep,
            &prompter,
        )
        .unwrap();
        assert!(matches!(action, ConflictAction::Replace));
        assert_eq!(prompter.requests().len(), 1);

        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Selected("keep".into())]);
        let action = resolve_conflict(
            &dst(),
            "macros",
            &mut sticky,
            NonInteractive::Bail,
            &prompter,
        )
        .unwrap();
        assert!(matches!(action, ConflictAction::Keep));
        assert_eq!(prompter.requests().len(), 1);
        assert_eq!(sticky, StickyMode::None);
    }

    #[test]
    #[serial]
    fn scripted_answers_drive_actions_and_sticky_transitions() {
        let _script = prompt_script::install(&["keep", "replace", "replace-all"]);
        let mut sticky = StickyMode::None;

        assert!(matches!(
            resolve("hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Keep
        ));
        assert_eq!(sticky, StickyMode::None);
        assert!(matches!(
            resolve("hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Replace
        ));
        assert_eq!(sticky, StickyMode::None);
        assert!(matches!(
            resolve("hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Replace
        ));
        assert_eq!(sticky, StickyMode::ReplaceAll);
        // The sticky answer now short-circuits: no further prompt is asked.
        assert!(matches!(
            resolve("hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Replace
        ));
        assert_eq!(prompt_script::prompts_asked(), 3);
    }

    #[test]
    #[serial]
    fn abort_answer_bails_with_the_bundle_abort_message() {
        let _script = prompt_script::install(&["abort"]);
        let mut sticky = StickyMode::None;

        let err = resolve("hooks", &mut sticky, NonInteractive::Keep).unwrap_err();

        assert!(
            err.to_string()
                .contains("Install aborted by user at conflict resolution."),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn should_replace_existing_skips_identical_content_without_prompting() {
        let _script = prompt_script::install(&[]);
        let dir = crate::utils::temp_file("conflict-identical-", "");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hook.sh");
        fs::write(&path, "same content").unwrap();

        let mut sticky = StickyMode::None;
        let replace = should_replace_existing(
            &path,
            "same content",
            "hooks",
            InstallMode::Prompt,
            &mut sticky,
            &Scripted,
        )
        .unwrap();

        let _ = fs::remove_dir_all(&dir);
        assert!(!replace);
        assert_eq!(prompt_script::prompts_asked(), 0);
    }

    #[test]
    #[serial]
    fn should_replace_existing_never_prompts_in_skip_or_force_mode() {
        let _script = prompt_script::install(&[]);
        let dir = crate::utils::temp_file("conflict-modes-", "");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hook.sh");
        fs::write(&path, "local edit").unwrap();

        let mut sticky = StickyMode::None;
        let skip = should_replace_existing(
            &path,
            "shipped",
            "hooks",
            InstallMode::Skip,
            &mut sticky,
            &Scripted,
        );
        let force = should_replace_existing(
            &path,
            "shipped",
            "hooks",
            InstallMode::Force,
            &mut sticky,
            &Scripted,
        );

        let _ = fs::remove_dir_all(&dir);
        assert!(!skip.unwrap());
        assert!(force.unwrap());
        assert_eq!(prompt_script::prompts_asked(), 0);
    }
}
