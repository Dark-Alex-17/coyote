use crate::config::RequestContext;

use parking_lot::RwLock;
use reedline::{Prompt, PromptHistorySearch, PromptHistorySearchStatus};
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Clone)]
pub struct ReplPrompt {
    ctx: Arc<RwLock<RequestContext>>,
}

impl ReplPrompt {
    pub fn new(ctx: Arc<RwLock<RequestContext>>) -> Self {
        Self { ctx }
    }
}

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let ctx = self.ctx.read();
        Cow::Owned(ctx.render_prompt_left(ctx.app.config.as_ref()))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        let ctx = self.ctx.read();
        Cow::Owned(ctx.render_prompt_right(ctx.app.config.as_ref()))
    }

    fn render_prompt_indicator(&self, _prompt_mode: reedline::PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        // NOTE: magic strings, given there is logic on how these are composed, I'm unsure if it's
        // worth extracting into a static constant
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            prefix, history_search.term
        ))
    }
}
