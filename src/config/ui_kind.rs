/// The surface that answers this run's prompts, exported to tools as
/// `COYOTE_UI` so a tool can tell whether a human can be asked at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiKind {
    Repl,
    Headless,
}

impl UiKind {
    /// The environment variable every tool process receives.
    pub(crate) const ENV: &'static str = "COYOTE_UI";

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Repl => "repl",
            Self::Headless => "headless",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, RequestContext, WorkingMode};
    use crate::testing::AtomicFlagGuard;
    use crate::utils::{ACP_SERVER, HEADLESS};
    use serial_test::serial;
    use std::sync::Arc;

    #[test]
    #[serial]
    fn ui_kind_follows_whether_a_human_is_present() {
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);

        let headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        assert_eq!(ctx.ui_kind(), UiKind::Repl);
        assert_eq!(ctx.ui_kind().as_str(), "repl");
        drop(headless_off);

        let _headless_on = AtomicFlagGuard::set(&HEADLESS, true);
        assert_eq!(ctx.ui_kind(), UiKind::Headless);
        assert_eq!(ctx.ui_kind().as_str(), "headless");

        let _acp_on = AtomicFlagGuard::set(&ACP_SERVER, true);
        assert_eq!(ctx.ui_kind(), UiKind::Headless);
    }
}
