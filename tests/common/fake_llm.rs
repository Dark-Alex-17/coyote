use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

pub enum Reply {
    ToolCall { name: String, arguments: String },
    Text(String),
}

/// An OpenAI chat-completions endpoint that answers the N-th POST with the
/// N-th scripted reply, in whichever form (`stream` or not) the request asks for.
pub struct FakeLlm {
    port: u16,
    served: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeLlm {
    pub fn start(replies: Vec<Reply>) -> FakeLlm {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let served = Arc::clone(&served);
            let stop = Arc::clone(&stop);
            move || serve(listener, &replies, &served, &stop)
        });
        FakeLlm {
            port,
            served,
            stop,
            thread: Some(thread),
        }
    }

    pub fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

impl Drop for FakeLlm {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // A connection is the only thing that returns from a blocking accept.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(listener: TcpListener, replies: &[Reply], served: &AtomicUsize, stop: &AtomicBool) {
    let fallback = Reply::Text("fake llm: no reply scripted".to_string());
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let mut stream = match conn {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("fake llm: accept failed: {err}");
                continue;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let response = match read_chat_request(&mut stream) {
            Ok(body) => {
                let n = served.fetch_add(1, Ordering::SeqCst);
                let reply = replies.get(n).unwrap_or(&fallback);
                if body["stream"] == Value::Bool(true) {
                    sse_response(reply)
                } else {
                    json_response(reply)
                }
            }
            Err(err) => {
                eprintln!("fake llm: rejecting request: {err}");
                http_response(400, "text/plain", err.to_string().as_bytes())
            }
        };
        let _ = stream.write_all(&response);
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// Reads one HTTP request and returns its JSON body if it is a POST to
/// the chat-completions path.
fn read_chat_request(stream: &mut TcpStream) -> io::Result<Value> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(invalid("connection closed before the request head ended"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if method != "POST" || !path.ends_with("/chat/completions") {
        return Err(invalid(format!("unexpected request line {request_line:?}")));
    }
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .ok_or_else(|| invalid("missing Content-Length"))?;
    let mut body = buf[head_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(invalid("connection closed before the body ended"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    serde_json::from_slice(&body).map_err(|err| invalid(format!("body is not JSON: {err}")))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn json_response(reply: &Reply) -> Vec<u8> {
    let (message, finish_reason) = match reply {
        Reply::Text(text) => (json!({"role": "assistant", "content": text}), "stop"),
        Reply::ToolCall { name, arguments } => (
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }],
            }),
            "tool_calls",
        ),
    };
    let body = json!({
        "id": "fake",
        "object": "chat.completion",
        "model": "fake-model",
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
    });
    http_response(200, "application/json", body.to_string().as_bytes())
}

fn sse_response(reply: &Reply) -> Vec<u8> {
    let delta = match reply {
        Reply::Text(text) => json!({"content": text}),
        Reply::ToolCall { name, arguments } => json!({
            "tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            }],
        }),
    };
    let frame = json!({"choices": [{"index": 0, "delta": delta}]});
    let payload = format!("data: {frame}\n\ndata: [DONE]\n\n");
    http_response(200, "text/event-stream", payload.as_bytes())
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        _ => "Bad Request",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}
