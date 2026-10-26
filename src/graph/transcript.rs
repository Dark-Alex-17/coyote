use crate::client::{Message, MessageContent, MessageContentPart};

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
/// `[N earlier turn(s) omitted]` line. That line counts toward `max_bytes`:
/// the newest exchange is kept whole when `max_bytes` covers that exchange
/// plus the omitted line; below that it is cut from the front at a char
/// boundary.
///
/// System rows are skipped on purpose: the role prompt sits at
/// `messages[0]` as a System row, and compression rewrites its summary into
/// that same row, so a compressed session renders only its kept tail and the
/// summary never reaches the graph. Tool rows and tool-call payloads are
/// skipped as well.
///
/// Exchanges are walked newest-first and only the kept ones are rendered, so
/// a long session costs the budget's worth of string building, not the
/// session's.
pub(crate) fn render_turns(messages: &[Message], max_bytes: usize) -> String {
    let mut exchanges = Exchanges::newest_first(messages);
    let Some(newest) = exchanges.next() else {
        return String::new();
    };
    // Newest first; the oldest kept exchange is always the last entry.
    let mut kept = vec![newest.render()];
    let mut body_len = kept[0].len();
    let mut dropped = 0;
    for exchange in exchanges.by_ref() {
        let len = exchange.len();
        if body_len + len + 1 > max_bytes {
            dropped += 1;
            break;
        }
        body_len += len + 1;
        kept.push(exchange.render());
    }
    dropped += exchanges.count();
    // The omitted line widens with its count, so one more drop can leave the
    // sum still over budget when the oldest kept exchange is shorter than it.
    while let [_, .., oldest] = kept.as_slice()
        && omitted_prefix(dropped).len() + body_len > max_bytes
    {
        body_len -= oldest.len() + 1;
        kept.pop();
        dropped += 1;
    }
    let prefix = omitted_prefix(dropped);
    kept.reverse();
    let body = kept.join("\n");
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

#[cfg(test)]
thread_local! {
    /// Counts `Exchange::render` calls so a test can prove that dropped
    /// exchanges are never built.
    static RENDERED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One exchange as a slice of the session: it runs from a renderable User
/// message (or the first message, for replies that precede any user turn)
/// up to the next renderable User message. Rows the transcript skips stay in
/// the slice and are filtered when it is rendered or measured.
struct Exchange<'a> {
    messages: &'a [Message],
}

impl Exchange<'_> {
    fn render(&self) -> String {
        #[cfg(test)]
        RENDERED.with(|count| count.set(count.get() + 1));
        self.messages
            .iter()
            .filter_map(render_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Byte length of `render()` without building it.
    fn len(&self) -> usize {
        let mut len = 0;
        let mut lines = 0usize;
        for message in self.messages {
            let Some(speaker) = speaker(message) else {
                continue;
            };
            len += speaker.len() + ": ".len() + text_len(&message.content);
            lines += 1;
        }
        len + lines.saturating_sub(1)
    }
}

struct Exchanges<'a> {
    messages: &'a [Message],
    /// Exclusive end of the oldest exchange not yet yielded.
    end: usize,
}

impl<'a> Exchanges<'a> {
    fn newest_first(messages: &'a [Message]) -> Self {
        Self {
            messages,
            end: messages.len(),
        }
    }
}

impl<'a> Iterator for Exchanges<'a> {
    type Item = Exchange<'a>;

    fn next(&mut self) -> Option<Exchange<'a>> {
        let end = self.end;
        let mut saw_reply = false;
        for start in (0..end).rev() {
            let message = &self.messages[start];
            if speaker(message).is_none() {
                continue;
            }
            if message.role.is_user() {
                self.end = start;
                return Some(Exchange {
                    messages: &self.messages[start..end],
                });
            }
            saw_reply = true;
        }
        self.end = 0;
        saw_reply.then(|| Exchange {
            messages: &self.messages[..end],
        })
    }
}

fn speaker(message: &Message) -> Option<&'static str> {
    let speaker = if message.role.is_user() {
        "user"
    } else if message.role.is_assistant() {
        "assistant"
    } else {
        return None;
    };
    match &message.content {
        MessageContent::ToolCalls(_) => None,
        _ => Some(speaker),
    }
}

fn render_line(message: &Message) -> Option<String> {
    speaker(message).map(|speaker| format!("{speaker}: {}", message.content.to_text()))
}

/// Mirrors `MessageContent::to_text`, which joins the text parts of an array
/// with a blank line.
fn text_len(content: &MessageContent) -> usize {
    match content {
        MessageContent::Text(text) => text.len(),
        MessageContent::Array(list) => {
            let mut len = 0;
            let mut parts = 0usize;
            for item in list {
                if let MessageContentPart::Text { text } = item {
                    len += text.len();
                    parts += 1;
                }
            }
            len + "\n\n".len() * parts.saturating_sub(1)
        }
        MessageContent::ToolCalls(_) => 0,
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
    use crate::client::{ImageUrl, MessageContentToolCalls, MessageRole, Model, ModelData};
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

    /// The forward algorithm this module replaced: every exchange is built
    /// oldest-first, then the oldest are dropped until the rest fit. Kept as
    /// the oracle for the newest-first walk. Returns the rendering and the
    /// number of exchanges kept.
    fn forward_render_turns(messages: &[Message], max_bytes: usize) -> (String, usize) {
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
        if exchanges.is_empty() {
            return (String::new(), 0);
        }
        let mut dropped = 0;
        let mut body_len = exchanges.iter().map(String::len).sum::<usize>() + exchanges.len() - 1;
        while dropped + 1 < exchanges.len() && omitted_prefix(dropped).len() + body_len > max_bytes
        {
            body_len -= exchanges[dropped].len() + 1;
            dropped += 1;
        }
        let kept = exchanges.len() - dropped;
        let prefix = omitted_prefix(dropped);
        let body = exchanges[dropped..].join("\n");
        if prefix.len() + body.len() <= max_bytes {
            return (format!("{prefix}{body}"), kept);
        }
        if prefix.len() >= max_bytes {
            return (truncate_from_front(&body, max_bytes), kept);
        }
        let rendered = format!(
            "{prefix}{}",
            truncate_from_front(&body, max_bytes - prefix.len())
        );
        (rendered, kept)
    }

    fn parts(texts: &[&str], with_image: bool) -> Message {
        let mut list: Vec<MessageContentPart> = texts
            .iter()
            .map(|text| MessageContentPart::Text {
                text: text.to_string(),
            })
            .collect();
        if with_image {
            list.push(MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,AA==".into(),
                },
            });
        }
        Message::new(MessageRole::User, MessageContent::Array(list))
    }

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
    fn the_newest_exchange_stays_whole_once_the_budget_covers_it_and_the_omitted_line() {
        let pad = "x".repeat(40);
        let messages = vec![
            text(MessageRole::User, &format!("q1 {pad}")),
            text(MessageRole::Assistant, "r1"),
            text(MessageRole::User, &format!("q2 {pad}")),
            text(MessageRole::Assistant, "r2"),
        ];
        let last = format!("user: q2 {pad}\nassistant: r2");
        let omitted_line = "[1 earlier turn(s) omitted]\n";
        let exact_fit = last.len() + omitted_line.len();

        assert_eq!(
            render_turns(&messages, exact_fit),
            format!("{omitted_line}{last}")
        );

        let one_short = exact_fit - 1;
        let rendered = render_turns(&messages, one_short);

        assert!(rendered.len() <= one_short, "{}", rendered.len());
        assert!(
            rendered.starts_with(&format!("{omitted_line}{TRUNCATION_MARK}")),
            "{rendered}"
        );
        assert!(rendered.ends_with("\nassistant: r2"), "{rendered}");
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

    #[test]
    fn an_exchange_measures_exactly_what_it_renders() {
        let messages = vec![
            text(MessageRole::System, "s"),
            parts(&["see", "this"], true),
            text(MessageRole::Tool, "tool text"),
            text(MessageRole::Assistant, "\u{4e2d}\u{4e2d}"),
            parts(&[], true),
            text(MessageRole::Assistant, "r"),
        ];
        let exchanges: Vec<Exchange> = Exchanges::newest_first(&messages).collect();

        assert_eq!(exchanges.len(), 2);
        for exchange in &exchanges {
            assert_eq!(exchange.len(), exchange.render().len());
        }
        assert_eq!(
            exchanges[1].render(),
            "user: see\n\nthis\nassistant: \u{4e2d}\u{4e2d}"
        );
        assert_eq!(exchanges[0].render(), "user: \nassistant: r");
    }

    #[test]
    fn replies_before_any_user_turn_form_the_oldest_exchange() {
        let messages = vec![
            text(MessageRole::System, "s"),
            text(MessageRole::Assistant, "hello"),
            text(MessageRole::Assistant, "again"),
            text(MessageRole::User, "q1"),
            text(MessageRole::Assistant, "r1"),
        ];

        let rendered: Vec<String> = Exchanges::newest_first(&messages)
            .map(|exchange| exchange.render())
            .collect();

        assert_eq!(
            rendered,
            [
                "user: q1\nassistant: r1",
                "assistant: hello\nassistant: again"
            ]
        );
        assert_eq!(
            render_turns(&messages, 1000),
            forward_render_turns(&messages, 1000).0
        );
    }

    fn ten_thousand_varied_exchanges() -> Vec<Message> {
        let mut messages = vec![text(MessageRole::System, "s")];
        for i in 0..10_000 {
            let filler = "x".repeat((i * 13) % 97);
            messages.push(text(MessageRole::User, &format!("q{i} {filler}")));
            if i % 7 == 0 {
                messages.push(text(MessageRole::Tool, "tool text"));
                messages.push(parts(&[&format!("part {i}"), "more"], true));
            }
            messages.push(text(MessageRole::Assistant, &format!("r{i} {filler}")));
            if i % 3 == 0 {
                messages.push(text(MessageRole::Assistant, "follow-up"));
            }
        }
        messages
    }

    fn ten_thousand_equal_width_exchanges() -> Vec<Message> {
        let mut messages = vec![text(MessageRole::System, "s")];
        let filler = "x".repeat(40);
        for i in 0..10_000 {
            messages.push(text(MessageRole::User, &format!("q{i:04} {filler}")));
            messages.push(text(MessageRole::Tool, "tool text"));
            messages.push(text(MessageRole::Assistant, &format!("r{i:04}")));
        }
        messages
    }

    #[test]
    fn ten_thousand_exchanges_render_like_the_forward_walk() {
        let messages = ten_thousand_varied_exchanges();

        for max_bytes in [PREVIOUS_TURNS_BYTES, 4096, 300, 64, 31, 10] {
            let (expected, _) = forward_render_turns(&messages, max_bytes);
            assert_eq!(render_turns(&messages, max_bytes), expected, "{max_bytes}");
        }
    }

    #[test]
    fn only_the_kept_exchanges_are_rendered() {
        // A budget whose slack over fifty whole exchanges covers the omitted
        // line, so the walk stops exactly where the forward algorithm does and
        // nothing it rendered is dropped again.
        let messages = ten_thousand_equal_width_exchanges();
        let width = Exchanges::newest_first(&messages).next().unwrap().len();
        let max_bytes = 50 * (width + 1) - 1 + 40;
        let (expected, kept) = forward_render_turns(&messages, max_bytes);
        RENDERED.with(|count| count.set(0));

        let rendered = render_turns(&messages, max_bytes);

        assert_eq!(rendered, expected);
        assert_eq!(kept, 50);
        assert!(rendered.starts_with("[9950 earlier turn(s) omitted]\n"));
        assert_eq!(RENDERED.with(|count| count.get()), kept);
    }
}
