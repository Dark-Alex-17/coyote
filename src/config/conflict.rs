//! Shared keep/replace conflict resolution for installers that must not
//! silently clobber locally modified files. The bundle installer and the
//! builtin hook installers all funnel per-file conflicts through
//! [`resolve_conflict`], so the prompt wording and the sticky
//! keep-all/replace-all semantics are identical everywhere — including when
//! one sticky scope spans several install locations in a single run.

use crate::utils::IS_STDOUT_TERMINAL;

use anyhow::{Context, Result, bail};
use inquire::Select;
use std::fs;
use std::path::Path;

/// How an installer treats a destination file that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallMode {
    /// Leave existing files untouched. The only mode startup ever uses:
    /// startup runs on every launch — including the REPL, which is a
    /// terminal — so it must never prompt.
    Skip,
    /// Overwrite existing files unconditionally.
    Force,
    /// Write missing files, silently skip identical ones, and ask per
    /// differing file.
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

/// What [`resolve_conflict`] does without a terminal: bundle installs bail
/// (an unattended install must not half-apply a plan silently), builtin hook
/// refreshes keep the local file (a scripted refresh must neither clobber
/// local changes nor fail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NonInteractive {
    Bail,
    Keep,
}

/// Decides whether an installer should overwrite the existing file at `dst`
/// with `content` under `mode`. Only called for files that already exist;
/// missing files are always written without prompting. In `Prompt` mode an
/// unreadable local file counts as differing, so the user decides.
pub(crate) fn should_replace_existing(
    dst: &Path,
    content: &str,
    category: &str,
    mode: InstallMode,
    sticky: &mut StickyMode,
) -> Result<bool> {
    match mode {
        InstallMode::Skip => Ok(false),
        InstallMode::Force => Ok(true),
        InstallMode::Prompt => {
            if fs::read(dst).is_ok_and(|local| local == content.as_bytes()) {
                return Ok(false);
            }
            match resolve_conflict(dst, category, sticky, NonInteractive::Keep)? {
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
) -> Result<ConflictAction> {
    match *sticky {
        StickyMode::KeepAll => return Ok(ConflictAction::Keep),
        StickyMode::ReplaceAll => return Ok(ConflictAction::Replace),
        StickyMode::None => {}
    }

    if !is_interactive() {
        match non_interactive {
            NonInteractive::Keep => return Ok(ConflictAction::Keep),
            NonInteractive::Bail => bail!(
                "Refusing to overwrite local file {} non-interactively. \
                 Re-run in a terminal, with --install-force (installs), \
                 or with --yes (updates).",
                dst.display()
            ),
        }
    }

    let prompt = format!("Conflict at {} (category: {category})", dst.display());
    let choice = select_choice(&prompt)?;

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
        _ => unreachable!("inquire::Select returned an unexpected option"),
    }
}

fn is_interactive() -> bool {
    #[cfg(test)]
    if let Some(forced) = prompt_script::forced_tty() {
        return forced;
    }
    *IS_STDOUT_TERMINAL
}

fn select_choice(prompt: &str) -> Result<String> {
    #[cfg(test)]
    if prompt_script::active() {
        return Ok(prompt_script::next_answer(prompt));
    }
    Select::new(
        prompt,
        vec!["keep", "replace", "keep-all", "replace-all", "abort"],
    )
    .prompt()
    .map(str::to_string)
    .with_context(|| "failed to read conflict choice")
}

#[cfg(test)]
pub(crate) mod prompt_script {
    //! Scripted stand-in for the interactive conflict prompt.
    //! `inquire::Select` cannot run under the test harness, so while a guard
    //! is installed the resolver takes its terminal answer from the guard and
    //! serves each prompt from a queue of scripted answers, counting every
    //! prompt asked. A forced-terminal guard with an empty queue pins the
    //! "never prompts" invariant: any prompt panics and the counter stays 0.

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

    pub(super) fn active() -> bool {
        STATE.load(Ordering::SeqCst) != INACTIVE
    }

    pub(super) fn forced_tty() -> Option<bool> {
        match STATE.load(Ordering::SeqCst) {
            TTY => Some(true),
            NON_TTY => Some(false),
            _ => None,
        }
    }

    pub(super) fn next_answer(prompt: &str) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    fn dst() -> PathBuf {
        PathBuf::from("/tmp/coyote-conflict-tests/example.yaml")
    }

    #[test]
    fn sticky_modes_short_circuit_without_prompting() {
        let mut sticky = StickyMode::KeepAll;
        assert!(matches!(
            resolve_conflict(&dst(), "macros", &mut sticky, NonInteractive::Bail).unwrap(),
            ConflictAction::Keep
        ));

        let mut sticky = StickyMode::ReplaceAll;
        assert!(matches!(
            resolve_conflict(&dst(), "macros", &mut sticky, NonInteractive::Bail).unwrap(),
            ConflictAction::Replace
        ));
    }

    #[test]
    #[serial]
    fn non_interactive_bail_keeps_the_bundle_error_shape() {
        let _script = prompt_script::install_non_interactive();
        let mut sticky = StickyMode::None;

        let err =
            resolve_conflict(&dst(), "macros", &mut sticky, NonInteractive::Bail).unwrap_err();

        assert!(
            err.to_string().contains("Refusing to overwrite local file"),
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

        let action = resolve_conflict(&dst(), "hooks", &mut sticky, NonInteractive::Keep).unwrap();

        assert!(matches!(action, ConflictAction::Keep));
        assert_eq!(sticky, StickyMode::None);
        assert_eq!(prompt_script::prompts_asked(), 0);
    }

    #[test]
    #[serial]
    fn scripted_answers_drive_actions_and_sticky_transitions() {
        let _script = prompt_script::install(&["keep", "replace", "replace-all"]);
        let mut sticky = StickyMode::None;

        assert!(matches!(
            resolve_conflict(&dst(), "hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Keep
        ));
        assert_eq!(sticky, StickyMode::None);
        assert!(matches!(
            resolve_conflict(&dst(), "hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Replace
        ));
        assert_eq!(sticky, StickyMode::None);
        assert!(matches!(
            resolve_conflict(&dst(), "hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Replace
        ));
        assert_eq!(sticky, StickyMode::ReplaceAll);
        // The sticky answer now short-circuits: no further prompt is asked.
        assert!(matches!(
            resolve_conflict(&dst(), "hooks", &mut sticky, NonInteractive::Keep).unwrap(),
            ConflictAction::Replace
        ));
        assert_eq!(prompt_script::prompts_asked(), 3);
    }

    #[test]
    #[serial]
    fn abort_answer_bails_with_the_bundle_abort_message() {
        let _script = prompt_script::install(&["abort"]);
        let mut sticky = StickyMode::None;

        let err = resolve_conflict(&dst(), "hooks", &mut sticky, NonInteractive::Keep).unwrap_err();

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
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hook.sh");
        std::fs::write(&path, "same content").unwrap();

        let mut sticky = StickyMode::None;
        let replace = should_replace_existing(
            &path,
            "same content",
            "hooks",
            InstallMode::Prompt,
            &mut sticky,
        )
        .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        assert!(!replace);
        assert_eq!(prompt_script::prompts_asked(), 0);
    }

    #[test]
    #[serial]
    fn should_replace_existing_never_prompts_in_skip_or_force_mode() {
        let _script = prompt_script::install(&[]);
        let dir = crate::utils::temp_file("conflict-modes-", "");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hook.sh");
        std::fs::write(&path, "local edit").unwrap();

        let mut sticky = StickyMode::None;
        let skip =
            should_replace_existing(&path, "shipped", "hooks", InstallMode::Skip, &mut sticky);
        let force =
            should_replace_existing(&path, "shipped", "hooks", InstallMode::Force, &mut sticky);

        let _ = std::fs::remove_dir_all(&dir);
        assert!(!skip.unwrap());
        assert!(force.unwrap());
        assert_eq!(prompt_script::prompts_asked(), 0);
    }
}
