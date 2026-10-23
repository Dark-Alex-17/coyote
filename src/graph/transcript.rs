use crate::client::{Message, MessageContent};

/// Byte budget for the `previous_turns` state key. It has to leave room in
/// the state size cap for everything else a graph carries.
pub(crate) const PREVIOUS_TURNS_BYTES: usize = 24 * 1024;

const _: () = assert!(
    PREVIOUS_TURNS_BYTES < super::MAX_STATE_SIZE_BYTES,
    "previous_turns is one key of the graph state and must fit under the state size cap"
);

pub(crate) const TRUNCATION_MARK: &str = "...";

/// The session's user/assistant text as `user: ...` / `assistant: ...` lines,
/// cut to `max_bytes` by dropping the oldest exchanges whole (an exchange is a
/// user message and the replies up to the next user message) behind an
/// `[N earlier turn(s) omitted]` line; only when the newest exchange alone
/// overflows is it cut from the front at a char boundary.
///
/// System rows are skipped on purpose: the role prompt sits at
/// `messages[0]` as a System row, and compression rewrites its summary into
/// that same row, so a compressed session renders only its kept tail and the
/// summary never reaches the graph. Tool rows and tool-call payloads are
/// skipped as well.
pub(crate) fn render_turns(messages: &[Message], max_bytes: usize) -> String {
    let exchanges = exchanges(messages);
    if exchanges.is_empty() {
        return String::new();
    }
    let mut dropped = 0;
    let mut body_len = exchanges.iter().map(String::len).sum::<usize>() + exchanges.len() - 1;
    while dropped + 1 < exchanges.len() && omitted_prefix(dropped).len() + body_len > max_bytes {
        body_len -= exchanges[dropped].len() + 1;
        dropped += 1;
    }
    let prefix = omitted_prefix(dropped);
    let body = exchanges[dropped..].join("\n");
    if prefix.len() + body.len() <= max_bytes {
        return format!("{prefix}{body}");
    }
    if prefix.len() >= max_bytes {
        return truncate_from_front(&body, max_bytes);
    }
    format!(
        "{prefix}{}",
        truncate_from_front(&body, max_bytes - prefix.len())
    )
}

fn omitted_prefix(dropped: usize) -> String {
    if dropped == 0 {
        String::new()
    } else {
        format!("[{dropped} earlier turn(s) omitted]\n")
    }
}

fn exchanges(messages: &[Message]) -> Vec<String> {
    let mut exchanges: Vec<String> = Vec::new();
    for message in messages {
        let Some(line) = render_line(message) else {
            continue;
        };
        match exchanges.last_mut() {
            Some(current) if !message.role.is_user() => {
                current.push('\n');
                current.push_str(&line);
            }
            _ => exchanges.push(line),
        }
    }
    exchanges
}

fn render_line(message: &Message) -> Option<String> {
    let speaker = if message.role.is_user() {
        "user"
    } else if message.role.is_assistant() {
        "assistant"
    } else {
        return None;
    };
    match &message.content {
        MessageContent::ToolCalls(_) => None,
        content => Some(format!("{speaker}: {}", content.to_text())),
    }
}

pub(crate) fn truncate_from_front(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let keep = max_bytes.saturating_sub(TRUNCATION_MARK.len());
    let mut cut = text.len() - keep;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("{TRUNCATION_MARK}{}", &text[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{MessageContentToolCalls, MessageRole, Model, ModelData};
    use crate::config::{AppState, Input, RequestContext, RoleLike, Session, WorkingMode};
    use crate::function::{ToolCall, ToolResult};
    use serde_json::json;
    use std::sync::Arc;

    fn text(role: MessageRole, text: &str) -> Message {
        Message::new(role, MessageContent::Text(text.to_string()))
    }

    fn conversation() -> Vec<Message> {
        vec![
            text(MessageRole::System, "s"),
            text(MessageRole::User, "q1"),
            text(MessageRole::Assistant, "r1"),
            text(MessageRole::User, "q2"),
            text(MessageRole::Assistant, "r2"),
        ]
    }

    const LAST_EXCHANGE: &str = "user: q2\nassistant: r2";
    const WHOLE: &str = "user: q1\nassistant: r1\nuser: q2\nassistant: r2";

    #[test]
    fn renders_user_and_assistant_lines_and_skips_system() {
        assert_eq!(render_turns(&conversation(), 1000), WHOLE);
    }

    #[test]
    fn skips_tool_rows_and_tool_call_payloads() {
        let tool_calls = MessageContentToolCalls::new(
            vec![ToolResult::new(
                ToolCall::new("t".into(), json!({}), Some("id-1".into())),
                json!("TOOL-OUTPUT"),
            )],
            "calling a tool".into(),
        );
        let messages = vec![
            text(MessageRole::User, "q1"),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(tool_calls.clone()),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::ToolCalls(tool_calls),
            ),
            text(MessageRole::Assistant, "r1"),
            text(MessageRole::Tool, "tool text"),
        ];

        assert_eq!(render_turns(&messages, 1000), "user: q1\nassistant: r1");
    }

    #[test]
    fn drops_the_oldest_exchange_whole_behind_an_omitted_line() {
        let pad = "x".repeat(40);
        let messages = vec![
            text(MessageRole::User, &format!("q1 {pad}")),
            text(MessageRole::Assistant, "r1"),
            text(MessageRole::User, &format!("q2 {pad}")),
            text(MessageRole::Assistant, "r2"),
        ];
        let last = format!("user: q2 {pad}\nassistant: r2");
        let whole = format!("user: q1 {pad}\nassistant: r1\n{last}");
        assert_eq!(render_turns(&messages, whole.len()), whole);

        let rendered = render_turns(&messages, whole.len() - 1);

        assert_eq!(rendered, format!("[1 earlier turn(s) omitted]\n{last}"));
        assert!(rendered.len() < whole.len());
    }

    #[test]
    fn counts_every_dropped_exchange() {
        let mut messages = conversation();
        messages.push(text(MessageRole::User, "q3"));
        messages.push(text(MessageRole::Assistant, "r3"));
        let last = "user: q3\nassistant: r3";
        let expected = format!("[2 earlier turn(s) omitted]\n{last}");

        assert_eq!(render_turns(&messages, expected.len()), expected);
    }

    #[test]
    fn the_omitted_line_counts_toward_the_budget() {
        let mut messages = conversation();
        messages.push(text(MessageRole::User, "q3"));
        messages.push(text(MessageRole::Assistant, "r3"));
        let whole = format!("{WHOLE}\nuser: q3\nassistant: r3");
        let two_kept = format!("{LAST_EXCHANGE}\nuser: q3\nassistant: r3");
        assert!(two_kept.len() < whole.len() - 1);

        assert_eq!(
            render_turns(&messages, whole.len() - 1),
            "[2 earlier turn(s) omitted]\nuser: q3\nassistant: r3"
        );
    }

    #[test]
    fn cuts_an_oversized_newest_exchange_from_the_front_at_a_char_boundary() {
        let huge = "\u{4e2d}".repeat(100);
        let messages = vec![
            text(MessageRole::User, "q1"),
            text(MessageRole::Assistant, "r1"),
            text(MessageRole::User, &huge),
            text(MessageRole::Assistant, "tail"),
        ];
        let max = 64;

        let rendered = render_turns(&messages, max);

        assert!(rendered.len() <= max, "{}", rendered.len());
        assert!(rendered.len() > max - 4, "{}", rendered.len());
        assert!(
            rendered.starts_with("[1 earlier turn(s) omitted]\n..."),
            "{rendered}"
        );
        assert!(rendered.ends_with("\nassistant: tail"), "{rendered}");
        let tail = rendered.trim_start_matches("[1 earlier turn(s) omitted]\n...");
        assert!(tail.chars().all(|c| c == '\u{4e2d}' || c.is_ascii()));
    }

    #[test]
    fn a_single_oversized_exchange_is_cut_without_an_omitted_line() {
        let huge = "\u{4e2d}".repeat(100);
        let messages = vec![
            text(MessageRole::User, &huge),
            text(MessageRole::Assistant, "tail"),
        ];
        let max = 32;

        let rendered = render_turns(&messages, max);

        assert!(rendered.len() <= max, "{}", rendered.len());
        assert!(rendered.starts_with("..."), "{rendered}");
        assert!(rendered.ends_with("\nassistant: tail"), "{rendered}");
        assert!(
            rendered[3..]
                .chars()
                .all(|c| c == '\u{4e2d}' || c.is_ascii())
        );
    }

    #[test]
    fn a_budget_under_the_omitted_line_emits_only_the_cut_tail() {
        let rendered = render_turns(&conversation(), 10);

        assert_eq!(rendered, "...ant: r2");
    }

    #[test]
    fn nothing_to_render_is_empty() {
        assert_eq!(render_turns(&[], 1000), "");
        assert_eq!(render_turns(&[text(MessageRole::System, "s")], 1000), "");
    }

    #[test]
    fn a_compressed_session_renders_only_its_kept_tail() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let mut session = Session::default();
        session.set_model(Model::from_config("openai", &[ModelData::new("fc-model")]).remove(0));
        ctx.session = Some(session);
        for i in 1..=3 {
            let input = Input::from_str(&ctx, &format!("q{i}"), None).unwrap();
            ctx.session
                .as_mut()
                .unwrap()
                .add_message(&input, &format!("r{i}"))
                .unwrap();
        }
        let keep = ctx.compression_keep_policy(Some(2));
        let session = ctx.session.as_mut().unwrap();

        session.compress("SUMMARY-TEXT".to_string(), keep);

        assert!(session.messages()[0].role.is_system());
        let rendered = render_turns(session.messages(), PREVIOUS_TURNS_BYTES);
        assert!(!rendered.contains("SUMMARY-TEXT"), "{rendered}");
        assert!(!rendered.contains("q1"), "{rendered}");
        assert_eq!(rendered, "user: q3\nassistant: r3");
    }
}
