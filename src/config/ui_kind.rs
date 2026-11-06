/// The surface that answers this run's prompts, exported to tools as
/// `COYOTE_UI` so a tool can tell whether a human can be asked at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiKind {
    Repl,
    Tui,
    Headless,
}

impl UiKind {
    /// The environment variable every child process receives: the tool,
    /// job and script-node env builders export this under `ENV`. On the
    /// way in, the config layer parses the same variable into `UiMode`,
    /// where `tui` selects the full-screen UI for an interactive run.
    pub(crate) const ENV: &'static str = "COYOTE_UI";

    /// The surface of this process right now: no human means headless,
    /// otherwise whichever UI owns the terminal. Every child-process env
    /// builder exports this under `ENV`.
    pub(crate) fn current() -> Self {
        if !crate::prompt::human_present() {
            Self::Headless
        } else if crate::tui::active() {
            Self::Tui
        } else {
            Self::Repl
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Repl => "repl",
            Self::Tui => "tui",
            Self::Headless => "headless",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, AppState, RequestContext, UiMode, WorkingMode};
    use crate::testing::AtomicFlagGuard;
    use crate::utils::{ACP_SERVER, HEADLESS};
    use serial_test::serial;
    use std::sync::Arc;

    #[test]
    #[serial]
    fn ui_kind_follows_whether_a_human_is_present() {
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);

        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        assert_eq!(ctx.ui_kind(), UiKind::Repl);
        assert_eq!(ctx.ui_kind().as_str(), "repl");

        let headless_on = AtomicFlagGuard::set(&HEADLESS, true);
        assert_eq!(ctx.ui_kind(), UiKind::Headless);
        assert_eq!(ctx.ui_kind().as_str(), "headless");
        drop(headless_on);
        assert_eq!(ctx.ui_kind(), UiKind::Repl);

        let _acp_on = AtomicFlagGuard::set(&ACP_SERVER, true);
        assert_eq!(ctx.ui_kind(), UiKind::Headless);
    }

    #[test]
    #[serial]
    fn ui_kind_is_tui_while_the_full_screen_ui_owns_the_terminal() {
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Repl);
        assert_eq!(ctx.ui_kind(), UiKind::Repl);

        let tui_on = AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        assert_eq!(ctx.ui_kind(), UiKind::Tui);
        assert_eq!(ctx.ui_kind().as_str(), "tui");
        drop(tui_on);
        assert_eq!(ctx.ui_kind(), UiKind::Repl);
    }

    /// No human is present when headless, even if the UI flag is somehow
    /// still set, so headless wins.
    #[test]
    #[serial]
    fn headless_outranks_the_tui_flag() {
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let _headless_on = AtomicFlagGuard::set(&HEADLESS, true);
        let _tui_on = AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Repl);
        assert_eq!(ctx.ui_kind(), UiKind::Headless);
    }

    /// An interactive run configured for the full-screen UI reports `Tui`
    /// before the loop has taken the terminal; a missing human still wins.
    #[test]
    #[serial]
    fn configured_tui_counts_as_tui_for_an_interactive_run() {
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        let app = AppState {
            config: Arc::new(AppConfig {
                ui: UiMode::Tui,
                ..AppConfig::default()
            }),
            ..AppState::test_default()
        };
        let ctx = RequestContext::new(Arc::new(app), WorkingMode::Repl);
        assert!(!crate::tui::active());
        assert_eq!(ctx.ui_kind(), UiKind::Tui);

        let _headless_on = AtomicFlagGuard::set(&HEADLESS, true);
        assert_eq!(ctx.ui_kind(), UiKind::Headless);
    }
}
