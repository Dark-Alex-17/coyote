use super::{ToolCall, catch_error};
use crate::utils::AbortSignal;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{Stream, StreamExt};
use reqwest::RequestBuilder;
use reqwest_eventsource::{Error as EventSourceError, Event, RequestBuilderExt};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

pub struct SseHandler {
    sender: UnboundedSender<SseEvent>,
    abort_signal: AbortSignal,
    buffer: String,
    tool_calls: Vec<ToolCall>,
    last_tool_calls: Vec<ToolCall>,
    max_call_repeats: usize,
    call_repeat_chain_len: usize,
}

impl SseHandler {
    pub fn new(sender: UnboundedSender<SseEvent>, abort_signal: AbortSignal) -> Self {
        Self {
            sender,
            abort_signal,
            buffer: String::new(),
            tool_calls: Vec::new(),
            last_tool_calls: Vec::new(),
            max_call_repeats: 2,
            call_repeat_chain_len: 3,
        }
    }

    pub fn text(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.buffer.push_str(text);
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
        if self.is_call_loop(&call) {
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

    pub fn abort(&self) -> AbortSignal {
        self.abort_signal.clone()
    }

    #[cfg(test)]
    pub fn last_tool_calls(&self) -> &[ToolCall] {
        &self.last_tool_calls
    }

    pub fn take(self) -> (String, Vec<ToolCall>) {
        let Self {
            buffer, tool_calls, ..
        } = self;
        (buffer, tool_calls)
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
    let mut es = builder.eventsource()?;
    while let Some(event) = es.next().await {
        match event {
            Ok(Event::Open) => {}
            Ok(Event::Message(message)) => {
                let message = SseMessage {
                    event: message.event,
                    data: message.data,
                };
                if handle(message)? {
                    break;
                }
            }
            Err(err) => {
                match err {
                    EventSourceError::StreamEnded => {}
                    EventSourceError::InvalidStatusCode(status, res) => {
                        let text = res.text().await?;
                        let data: Value = match text.parse() {
                            Ok(data) => data,
                            Err(_) => {
                                bail!(
                                    "Invalid response data: {text} (status: {})",
                                    status.as_u16()
                                );
                            }
                        };
                        catch_error(&data, status.as_u16())?;
                    }
                    EventSourceError::InvalidContentType(header_value, res) => {
                        let text = res.text().await?;
                        bail!(
                            "Invalid response event-stream. content-type: {}, data: {text}",
                            header_value.to_str().unwrap_or_default()
                        );
                    }
                    _ => {
                        bail!("{}", err);
                    }
                }
                es.close();
            }
        }
    }
    Ok(())
}

pub async fn json_stream<S, F, E>(mut stream: S, mut handle: F) -> Result<()>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    F: FnMut(&str) -> Result<()>,
    E: std::error::Error,
{
    let mut parser = JsonStreamParser::default();
    let mut unparsed_bytes = vec![];
    while let Some(chunk_bytes) = stream.next().await {
        let chunk_bytes =
            chunk_bytes.map_err(|err| anyhow!("Failed to read json stream, {err}"))?;
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
                '[' => {
                    if self.start.is_some() {
                        self.balances.push(ch);
                    }
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
    use rand::Rng;
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

    fn split_chunks(text: &str) -> Vec<Vec<u8>> {
        let mut rng = rand::rng();
        let len = text.len();
        let cut1 = rng.random_range(1..len - 1);
        let cut2 = rng.random_range(cut1 + 1..len);
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
