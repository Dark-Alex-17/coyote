use super::UiSink;
use crate::client::SseEvent;
use crate::config::AppConfig;
use crate::events::{NoticeKind, OutputStream};
use crate::function::format_call_log;
use crate::utils::{
    AbortSignal, IS_STDOUT_TERMINAL, Spinner, dimmed_text, muted_warning_text, spawn_spinner,
};
use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;

/// The terminal REPL's sink: prints straight to the process's stdout and stderr.
#[derive(Default)]
pub struct StdoutSink {
    spinner: Mutex<Option<Spinner>>,
}

fn echo_chunk(out: &mut impl Write, chunk: &[u8]) {
    let n = chunk.len();
    let mut last_pos = 0;
    for (i, &byte) in chunk.iter().enumerate() {
        if byte == b'\n' {
            let _ = out.write_all(&chunk[last_pos..i]);
            let _ = out.write_all(b"\r\n");
            last_pos = i + 1;
        }
    }
    if last_pos < n {
        let _ = out.write_all(&chunk[last_pos..n]);
    }
    let _ = out.flush();
}

/// A spinner left running past its sink would keep painting over whatever
/// the terminal shows next.
impl Drop for StdoutSink {
    fn drop(&mut self) {
        if let Some(spinner) = self
            .spinner
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            spinner.stop();
        }
    }
}

impl UiSink for StdoutSink {
    fn assistant_text(&self, app: &AppConfig, text: &str) -> Result<()> {
        app.print_markdown(text)
    }

    fn stream<'a>(
        &'a self,
        app: &'a AppConfig,
        rx: UnboundedReceiver<SseEvent>,
        abort: AbortSignal,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(crate::render::render_stream(rx, app, abort, false))
    }

    fn notice(&self, kind: NoticeKind, text: &str) {
        match kind {
            NoticeKind::Routing => println!("{text}"),
            NoticeKind::ToolWarning => eprintln!("{}", muted_warning_text(text)),
            NoticeKind::Checkpoint
            | NoticeKind::AutoContinue
            | NoticeKind::Autoname
            | NoticeKind::Interrupted
            | NoticeKind::Compress
            | NoticeKind::GraphNode => eprintln!("{text}"),
        }
    }

    fn tool_narration(
        &self,
        _call_id: &str,
        cmd_name: &str,
        cmd_args: &[String],
        json_data: &Value,
    ) {
        println!("{}", format_call_log(cmd_name, cmd_args, json_data));
    }

    // Subagent (depth > 0) tools run while the parent session owns the
    // terminal, so only the root's tools echo.
    fn tool_output(
        &self,
        _call_id: Option<&str>,
        stream: OutputStream,
        depth: usize,
        chunk: &[u8],
    ) {
        if depth > 0 {
            return;
        }
        match stream {
            OutputStream::Stdout => echo_chunk(&mut io::stdout(), chunk),
            OutputStream::Stderr => echo_chunk(&mut io::stderr(), chunk),
        }
    }

    fn busy(&self, message: Option<&str>) {
        if !*IS_STDOUT_TERMINAL {
            return;
        }
        let mut spinner = self
            .spinner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match message {
            Some(msg) => match spinner.as_ref() {
                Some(spinner) => {
                    let _ = spinner.set_message(msg.to_string());
                }
                None => *spinner = Some(spawn_spinner(msg)),
            },
            None => {
                if let Some(spinner) = spinner.take() {
                    spinner.stop();
                }
            }
        }
    }

    fn turn_divider(&self) {
        let width = crossterm::terminal::size()
            .map(|(columns, _)| columns as usize)
            .unwrap_or(33);
        println!("{}", dimmed_text(&"─".repeat(width))); // debris-exempt
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echoed(chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        echo_chunk(&mut out, chunk);
        out
    }

    #[test]
    fn echo_chunk_turns_every_newline_into_crlf() {
        assert_eq!(echoed(b"a\nb\n\nc"), b"a\r\nb\r\n\r\nc");
        assert_eq!(echoed(b"\n"), b"\r\n");
    }

    #[test]
    fn echo_chunk_passes_a_newline_free_chunk_through() {
        assert_eq!(echoed(b"no newline here"), b"no newline here");
    }

    #[test]
    fn echo_chunk_writes_nothing_for_an_empty_chunk() {
        assert_eq!(echoed(b""), b"");
    }

    #[cfg(unix)]
    mod fd_one {
        use super::super::*;
        use crate::testing::{capture_stdout, read_all, write_fd1};
        use crate::turn::SilentSink;
        use serial_test::serial;

        // The harness reports from other threads on fd 1 too, so the capture
        // is checked for our marker and then passed through to the real fd 1.
        // `notice` is not pinned this way: `println!`/`eprintln!` go through
        // the test harness's own output capture and never reach fd 1 or
        // fd 2 here, so its stream split is checked by the binary goldens.
        fn assert_marker_never_landed(sink: &dyn UiSink, depth: usize, marker: &str) {
            let (saved, mut reader) = capture_stdout();
            sink.tool_output(
                Some("call-1"),
                OutputStream::Stdout,
                depth,
                marker.as_bytes(),
            );
            drop(saved);
            let out = read_all(&mut reader);
            assert!(!out.contains(marker), "{out:?}");
            write_fd1(out.as_bytes());
        }

        #[test]
        #[serial]
        fn stdout_sink_at_depth_one_writes_nothing() {
            assert_marker_never_landed(&StdoutSink::default(), 1, "stdout-sink-depth-one-x7x\n");
        }

        #[test]
        #[serial]
        fn silent_sink_at_depth_zero_writes_nothing() {
            assert_marker_never_landed(&SilentSink, 0, "silent-sink-depth-zero-x7x\n");
        }
    }
}
