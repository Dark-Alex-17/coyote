use super::{ApiStatusError, ThinkingBlock, TokenUsage, ToolCall, catch_error};
use crate::utils::AbortSignal;

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::{Stream, StreamExt};
use reqwest::{RequestBuilder, header};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

pub struct SseHandler {
    sender: UnboundedSender<SseEvent>,
    abort_signal: AbortSignal,
    buffer: String,
    tool_calls: Vec<ToolCall>,
    thinking: Vec<ThinkingBlock>,
    usage: Option<TokenUsage>,
    last_tool_calls: Vec<ToolCall>,
    max_call_repeats: usize,
    call_repeat_chain_len: usize,
    silent: bool,
    call_loop_detection: bool,
}

impl SseHandler {
    pub fn new(sender: UnboundedSender<SseEvent>, abort_signal: AbortSignal) -> Self {
        Self {
            sender,
            abort_signal,
            buffer: String::new(),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            usage: None,
            last_tool_calls: Vec::new(),
            max_call_repeats: 2,
            call_repeat_chain_len: 3,
            silent: false,
            call_loop_detection: true,
        }
    }

    pub fn set_silent(&mut self, silent: bool) {
        self.silent = silent;
    }

    /// Loop detection guards an interactive stream; the non-streaming
    /// transport never had it, so callers that must match that transport's
    /// tool-call handling turn it off.
    pub fn set_call_loop_detection(&mut self, enabled: bool) {
        self.call_loop_detection = enabled;
    }

    pub fn text(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.buffer.push_str(text);

        if self.silent {
            return Ok(());
        }

        let ret = self
            .sender
            .send(SseEvent::Text(text.to_string()))
            .with_context(|| "Failed to send SseEvent:Text");
        if let Err(err) = ret {
            if self.abort_signal.aborted() {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    pub fn done(&mut self) {
        let ret = self.sender.send(SseEvent::Done);
        if ret.is_err() {
            if self.abort_signal.aborted() {
                return;
            }
            warn!("Failed to send SseEvent:Done");
        }
    }

    pub fn tool_call(&mut self, call: ToolCall) -> Result<()> {
        if self.call_loop_detection && self.is_call_loop(&call) {
            let loop_message = self.create_loop_detection_message(&call);
            return Err(anyhow!(loop_message));
        }

        if self.last_tool_calls.len() == self.call_repeat_chain_len * self.max_call_repeats {
            self.last_tool_calls.remove(0);
        }
        self.last_tool_calls.push(call.clone());

        self.tool_calls.push(call);

        Ok(())
    }

    fn is_call_loop(&self, new_call: &ToolCall) -> bool {
        if self.last_tool_calls.len() < self.call_repeat_chain_len {
            return false;
        }

        if let Some(last_call) = self.last_tool_calls.last()
            && self.calls_match(last_call, new_call)
        {
            let mut repeat_count = 1;
            for i in (0..self.last_tool_calls.len()).rev() {
                if i == 0 {
                    break;
                }
                if self.calls_match(&self.last_tool_calls[i - 1], &self.last_tool_calls[i]) {
                    repeat_count += 1;
                    if repeat_count >= self.max_call_repeats {
                        return true;
                    }
                } else {
                    break;
                }
            }
        }

        let chain_start = self
            .last_tool_calls
            .len()
            .saturating_sub(self.call_repeat_chain_len);
        let chain = &self.last_tool_calls[chain_start..];

        if chain.len() == self.call_repeat_chain_len {
            let mut is_repeating = true;
            for i in 0..chain.len() - 1 {
                if !self.calls_match(&chain[i], &chain[i + 1]) {
                    is_repeating = false;
                    break;
                }
            }
            if is_repeating && self.calls_match(&chain[chain.len() - 1], new_call) {
                return true;
            }
        }

        false
    }

    fn calls_match(&self, call1: &ToolCall, call2: &ToolCall) -> bool {
        call1.name == call2.name && call1.arguments == call2.arguments
    }

    fn create_loop_detection_message(&self, new_call: &ToolCall) -> String {
        let mut message = String::from("⚠️ Call loop detected! ⚠️");

        message.push_str(&format!(
            "The call '{}' with arguments '{}' is repeating.\n",
            new_call.name, new_call.arguments
        ));

        if self.last_tool_calls.len() >= self.call_repeat_chain_len {
            let chain_start = self
                .last_tool_calls
                .len()
                .saturating_sub(self.call_repeat_chain_len);
            let chain = &self.last_tool_calls[chain_start..];

            message.push_str("The following sequence of calls is repeating:\n");
            for (i, call) in chain.iter().enumerate() {
                message.push_str(&format!(
                    "  {}. {} with arguments {}\n",
                    i + 1,
                    call.name,
                    call.arguments
                ));
            }
        }

        message.push_str("\nPlease move on to the next task in your sequence using the last output you got from the call or chain you are trying to re-execute. ");
        message.push_str(
            "Consider using different parameters or a different approach to avoid this loop.",
        );

        message
    }

    pub fn thinking_block(&mut self, block: ThinkingBlock) {
        self.thinking.push(block);
    }

    /// Merges a usage update into the accumulator; later non-None fields win.
    /// Claude's `message_start` carries input/cache counts while
    /// `message_delta` carries the output count, so a single event never has
    /// the full picture.
    pub fn usage(&mut self, update: TokenUsage) {
        let usage = self.usage.get_or_insert_with(TokenUsage::default);
        if update.input_tokens.is_some() {
            usage.input_tokens = update.input_tokens;
        }
        if update.output_tokens.is_some() {
            usage.output_tokens = update.output_tokens;
        }
        if update.cache_creation_input_tokens.is_some() {
            usage.cache_creation_input_tokens = update.cache_creation_input_tokens;
        }
        if update.cache_read_input_tokens.is_some() {
            usage.cache_read_input_tokens = update.cache_read_input_tokens;
        }
    }

    /// Whether any output (text, tool calls, or thinking blocks) has been
    /// accumulated. `Client::chat_completions_streaming` gates its 401 retry
    /// on this: content already streamed to the user would be rendered a
    /// second time by a retry, so partial responses are never retried.
    pub fn has_received_content(&self) -> bool {
        !self.buffer.is_empty() || !self.tool_calls.is_empty() || !self.thinking.is_empty()
    }

    /// Whether visible output (text or tool calls) has been accumulated.
    /// Unlike [`Self::has_received_content`], thinking blocks do not count:
    /// a stream truncated before any visible output is still an empty
    /// response to the caller.
    pub fn has_received_visible_output(&self) -> bool {
        !self.buffer.is_empty() || !self.tool_calls.is_empty()
    }

    pub fn abort(&self) -> AbortSignal {
        self.abort_signal.clone()
    }

    #[cfg(test)]
    pub fn last_tool_calls(&self) -> &[ToolCall] {
        &self.last_tool_calls
    }

    pub fn take(
        self,
    ) -> (
        String,
        Vec<ToolCall>,
        Vec<ThinkingBlock>,
        Option<TokenUsage>,
    ) {
        let Self {
            buffer,
            tool_calls,
            thinking,
            usage,
            ..
        } = self;
        (buffer, tool_calls, thinking, usage)
    }
}

#[derive(Debug)]
pub enum SseEvent {
    Text(String),
    Done,
}

#[derive(Debug)]
pub struct SseMessage {
    #[allow(unused)]
    pub event: String,
    pub data: String,
}

pub async fn sse_stream<F>(builder: RequestBuilder, mut handle: F) -> Result<()>
where
    F: FnMut(SseMessage) -> Result<bool>,
{
    let res = builder
        .header(header::ACCEPT, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .send()
        .await?;
    let status = res.status();
    if !status.is_success() {
        let text = res.text().await?;
        let data: Value = match text.parse() {
            Ok(data) => data,
            Err(_) => {
                return Err(ApiStatusError {
                    status: status.as_u16(),
                    message: format!(
                        "Invalid response data: {text} (status: {})",
                        status.as_u16()
                    ),
                }
                .into());
            }
        };
        catch_error(&data, status.as_u16())?;
        return Ok(());
    }

    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let is_event_stream = content_type
        .as_deref()
        .map(|ct| ct.is_empty() || ct.starts_with("text/event-stream"))
        .unwrap_or(true);
    if !is_event_stream {
        let header_value = content_type.unwrap_or_default();
        let text = res.text().await?;
        bail!("Invalid response event-stream. content-type: {header_value}, data: {text}");
    }

    let mut es = res.bytes_stream().boxed().eventsource();
    while let Some(event) = es.next().await {
        match event {
            Ok(message) => {
                let message = SseMessage {
                    event: message.event,
                    data: message.data,
                };
                if handle(message)? {
                    break;
                }
            }
            // Keep the typed reqwest error in the chain: a read_timeout stall
            // is only recognised as transient via `reqwest::Error::is_timeout`.
            Err(EventStreamError::Transport(err)) => {
                return Err(err).context("Transport error");
            }
            Err(err) => {
                bail!("{err}");
            }
        }
    }
    Ok(())
}

pub async fn json_stream<S, F, E>(mut stream: S, mut handle: F) -> Result<()>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    F: FnMut(&str) -> Result<()>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut parser = JsonStreamParser::default();
    let mut unparsed_bytes = vec![];
    while let Some(chunk_bytes) = stream.next().await {
        let chunk_bytes = chunk_bytes.context("Failed to read json stream")?;
        unparsed_bytes.extend(chunk_bytes);
        match std::str::from_utf8(&unparsed_bytes) {
            Ok(text) => {
                parser.process(text, &mut handle)?;
                unparsed_bytes.clear();
            }
            Err(_) => {
                continue;
            }
        }
    }
    if !unparsed_bytes.is_empty() {
        let text = std::str::from_utf8(&unparsed_bytes)?;
        parser.process(text, &mut handle)?;
    }

    Ok(())
}

#[derive(Debug, Default)]
struct JsonStreamParser {
    buffer: Vec<char>,
    cursor: usize,
    start: Option<usize>,
    balances: Vec<char>,
    quoting: bool,
    escape: bool,
}

impl JsonStreamParser {
    fn process<F>(&mut self, text: &str, handle: &mut F) -> Result<()>
    where
        F: FnMut(&str) -> Result<()>,
    {
        self.buffer.extend(text.chars());

        for i in self.cursor..self.buffer.len() {
            let ch = self.buffer[i];
            if self.quoting {
                if ch == '\\' {
                    self.escape = !self.escape;
                } else {
                    if !self.escape && ch == '"' {
                        self.quoting = false;
                    }
                    self.escape = false;
                }
                continue;
            }
            match ch {
                '"' => {
                    self.quoting = true;
                    self.escape = false;
                }
                '{' => {
                    if self.balances.is_empty() {
                        self.start = Some(i);
                    }
                    self.balances.push(ch);
                }
                '[' if self.start.is_some() => {
                    self.balances.push(ch);
                }
                '}' => {
                    self.balances.pop();
                    if self.balances.is_empty()
                        && let Some(start) = self.start.take()
                    {
                        let value: String = self.buffer[start..=i].iter().collect();
                        handle(&value)?;
                    }
                }
                ']' => {
                    self.balances.pop();
                }
                _ => {}
            }
        }
        self.cursor = self.buffer.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use futures_util::stream;
    use rand::random_range;
    use serde_json::json;

    #[test]
    fn test_last_tool_calls_ring_buffer() {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);

        for i in 0..15 {
            let call = ToolCall::new(format!("test_function_{}", i), json!({"param": i}), None);
            handler.tool_call(call.clone()).unwrap();
        }
        let lt_len = handler.call_repeat_chain_len * handler.max_call_repeats;
        assert_eq!(handler.last_tool_calls().len(), lt_len);

        assert_eq!(
            handler.last_tool_calls()[lt_len - 1].name,
            "test_function_14"
        );

        assert_eq!(
            handler.last_tool_calls()[0].name,
            format!("test_function_{}", 14 - lt_len + 1)
        );
    }

    #[test]
    fn test_call_loop_detection() {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        let mut handler = SseHandler::new(sender, abort_signal);

        handler.max_call_repeats = 2;
        handler.call_repeat_chain_len = 3;

        let call = ToolCall::new("test_function_loop".to_string(), json!({"param": 1}), None);

        for _ in 0..3 {
            handler.tool_call(call.clone()).unwrap();
        }

        let result = handler.tool_call(call.clone());
        assert!(result.is_err());
        let error_message = result.unwrap_err().to_string();
        assert!(error_message.contains("Call loop detected!"));
        assert!(error_message.contains("test_function_loop"));
    }

    /// With detection off, repeated identical calls all accumulate — the
    /// non-streaming transport's behaviour, which the quiet transport mirrors.
    #[test]
    fn test_call_loop_detection_can_be_disabled() {
        let (mut handler, _rx) = new_handler();
        handler.set_call_loop_detection(false);
        let call = ToolCall::new("test_function_loop".to_string(), json!({"param": 1}), None);

        for _ in 0..5 {
            handler.tool_call(call.clone()).unwrap();
        }

        let (_, calls, _, _) = handler.take();
        assert_eq!(calls.len(), 5);
    }

    /// A transport failure keeps its typed error in the chain so callers can
    /// classify it (a stall is only recognised as transient via the type).
    #[tokio::test]
    async fn json_stream_keeps_the_typed_transport_error() {
        let stalled = std::io::Error::new(std::io::ErrorKind::TimedOut, "stall");
        let mut source = stream::iter(vec![Err::<Bytes, std::io::Error>(stalled)]);

        let err = json_stream(&mut source, |_| Ok(()))
            .await
            .expect_err("a failed read must fail the stream");

        assert!(err.chain().any(|c| {
            c.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
        }));
        assert!(format!("{err:#}").starts_with("Failed to read json stream"));
    }

    fn new_handler() -> (SseHandler, tokio::sync::mpsc::UnboundedReceiver<SseEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let abort_signal = crate::utils::create_abort_signal();
        (SseHandler::new(sender, abort_signal), receiver)
    }

    #[test]
    fn test_has_received_content_text() {
        let (mut handler, _rx) = new_handler();
        assert!(!handler.has_received_content());

        handler.text("hello").unwrap();
        assert!(handler.has_received_content());
    }

    #[test]
    fn test_has_received_content_tool_call() {
        let (mut handler, _rx) = new_handler();
        assert!(!handler.has_received_content());

        let call = ToolCall::new("test_function".to_string(), json!({"param": 1}), None);
        handler.tool_call(call).unwrap();
        assert!(handler.has_received_content());
    }

    #[test]
    fn test_has_received_content_thinking() {
        let (mut handler, _rx) = new_handler();
        assert!(!handler.has_received_content());

        handler.thinking_block(ThinkingBlock::Thinking {
            thinking: "hmm".to_string(),
            signature: "sig".to_string(),
        });
        assert!(handler.has_received_content());
    }

    /// Pins the deliberate exclusion: thinking blocks count as received
    /// content but not as visible output, so a thinking-only stream is
    /// still an empty response to the caller.
    #[test]
    fn thinking_blocks_do_not_count_as_visible_output() {
        let (mut handler, _rx) = new_handler();
        handler.thinking_block(ThinkingBlock::Thinking {
            thinking: "hmm".to_string(),
            signature: "sig".to_string(),
        });
        assert!(handler.has_received_content());
        assert!(!handler.has_received_visible_output());

        handler.text("hello").unwrap();
        assert!(handler.has_received_visible_output());
    }

    /// Claude splits usage across events: `message_start` carries the input
    /// and cache counts with a provisional output count, `message_delta`
    /// carries only the final output count. Later non-None fields must win.
    #[test]
    fn test_usage_merges_later_non_none_fields() {
        let (mut handler, _rx) = new_handler();

        handler.usage(TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(1),
            cache_creation_input_tokens: Some(200),
            cache_read_input_tokens: Some(300),
        });
        handler.usage(TokenUsage {
            output_tokens: Some(42),
            ..Default::default()
        });

        let (_, _, _, usage) = handler.take();
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(42));
        assert_eq!(usage.cache_creation_input_tokens, Some(200));
        assert_eq!(usage.cache_read_input_tokens, Some(300));
    }

    /// A silent handler is the quiet transport's whole output gate: text,
    /// tool calls, and thinking still accumulate for `take()`, but the
    /// receiver sees no `Text` events, only the trailing `Done`.
    #[test]
    fn test_silent_handler_accumulates_without_emitting_text() {
        let (mut handler, mut rx) = new_handler();
        handler.set_silent(true);

        handler.text("hel").unwrap();
        handler.text("lo").unwrap();
        handler.thinking_block(ThinkingBlock::Thinking {
            thinking: "hmm".to_string(),
            signature: "sig".to_string(),
        });
        handler
            .tool_call(ToolCall::new(
                "lookup".to_string(),
                json!({"q": 1}),
                Some("call-1".to_string()),
            ))
            .unwrap();
        handler.done();

        assert!(matches!(rx.try_recv(), Ok(SseEvent::Done)));
        assert!(rx.try_recv().is_err(), "no events beyond Done");

        let (text, calls, thinking, _) = handler.take();
        assert_eq!(text, "hello");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "lookup");
        assert_eq!(calls[0].id.as_deref(), Some("call-1"));
        assert_eq!(thinking.len(), 1);
        assert!(
            matches!(&thinking[0], ThinkingBlock::Thinking { thinking, .. } if thinking == "hmm")
        );
    }

    fn split_chunks(text: &str) -> Vec<Vec<u8>> {
        let len = text.len();
        let cut1 = random_range(1..len - 1);
        let cut2 = random_range(cut1 + 1..len);
        let chunk1 = text.as_bytes()[..cut1].to_vec();
        let chunk2 = text.as_bytes()[cut1..cut2].to_vec();
        let chunk3 = text.as_bytes()[cut2..].to_vec();
        vec![chunk1, chunk2, chunk3]
    }

    macro_rules! assert_json_stream {
        ($input:expr, $output:expr) => {
            let chunks: Vec<_> = split_chunks($input)
                .into_iter()
                .map(|chunk| Ok::<_, std::convert::Infallible>(Bytes::from(chunk)))
                .collect();
            let stream = stream::iter(chunks);
            let mut output = vec![];
            let ret = json_stream(stream, |data| {
                output.push(data.to_string());
                Ok(())
            })
            .await;
            assert!(ret.is_ok());
            assert_eq!($output.replace("\r\n", "\n"), output.join("\n"))
        };
    }

    #[tokio::test]
    async fn test_json_stream_ndjson() {
        let data = r#"{"key": "value"}
{"key": "value2"}
{"key": "value3"}"#;
        assert_json_stream!(data, data);
    }

    #[tokio::test]
    async fn test_json_stream_array() {
        let input = r#"[
{"key": "value"},
{"key": "value2"},
{"key": "value3"},"#;
        let output = r#"{"key": "value"}
{"key": "value2"}
{"key": "value3"}"#;
        assert_json_stream!(input, output);
    }
}
