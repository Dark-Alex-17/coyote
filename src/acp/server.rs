use super::types::{METHOD_NOT_FOUND, PARSE_ERROR, Request, Response};
use crate::client::call_chat_completions_streaming;
use crate::config::{Input, RenderMode, RequestContext};
use crate::utils;
use crate::utils::AbortSignal;
use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub(crate) struct AcpServerState {
    ctx: Option<RequestContext>,
    abort: AbortSignal,
    session_active: bool,
}

pub async fn run_acp_server(ctx: RequestContext, abort: AbortSignal) -> Result<()> {
    let state = AcpServerState {
        ctx: Some(ctx),
        abort,
        session_active: false,
    };

    run_acp_server_with_state(tokio::io::stdin(), tokio::io::stdout(), state).await
}

#[cfg(test)]
pub(crate) async fn run_acp_server_on<R, W>(reader: R, writer: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use crate::utils::{create_abort_signal, drain_acp_permissions};

    drain_acp_permissions();
    let state = AcpServerState {
        ctx: None,
        abort: create_abort_signal(),
        session_active: false,
    };

    run_acp_server_with_state(reader, writer, state).await
}

async fn run_acp_server_with_state<R, W>(
    reader: R,
    mut writer: W,
    mut state: AcpServerState,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reader = BufReader::new(reader);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        if let Some(response) = dispatch(&line, &mut state).await {
            for params in utils::drain_acp_permissions() {
                emit_notification(&mut writer, "session/request_permission", params).await?;
            }

            emit(&mut writer, &response).await?;
        }
    }

    Ok(())
}

async fn dispatch(raw: &str, state: &mut AcpServerState) -> Option<Response> {
    let req: Request = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(_) => return Some(Response::err(None, PARSE_ERROR, "Parse error")),
    };

    // session/cancel is a notification. Handle it regardless of whether an id is present.
    if req.method == "session/cancel" {
        handle_session_cancel(state);
        return if req.id.is_some() {
            Some(Response::ok(req.id, json!({})))
        } else {
            None
        };
    }

    req.id.as_ref()?;

    Some(match req.method.as_str() {
        "initialize" => handle_initialize(req),
        "session/new" => handle_session_new(req, state).await,
        "session/load" => handle_session_load(req, state).await,
        "session/prompt" => handle_session_prompt(req, state).await,
        _ => Response::err(
            req.id,
            METHOD_NOT_FOUND,
            format!("Method not found: {}", req.method),
        ),
    })
}

fn handle_initialize(req: Request) -> Response {
    Response::ok(
        req.id,
        json!({
            "name": "coyote",
            "version": env!("CARGO_PKG_VERSION"),
            "protocolVersion": "1",
        }),
    )
}

async fn handle_session_new(req: Request, state: &mut AcpServerState) -> Response {
    if state.session_active {
        return Response::err(
            req.id,
            -32000,
            "Session already active; this server supports one session per process",
        );
    }

    let ctx = match state.ctx.as_mut() {
        Some(c) => c,
        None => {
            state.session_active = true;
            return Response::ok(req.id, json!({ "sessionId": "default" }));
        }
    };

    let app = Arc::clone(&ctx.app.config);
    let abort = state.abort.clone();
    match ctx.use_session(app.as_ref(), None, abort).await {
        Ok(_) => {
            state.session_active = true;
            ctx.render_mode = RenderMode::Silent;
            Response::ok(req.id, json!({ "sessionId": "default" }))
        }
        Err(e) => Response::err(req.id, -32000, format!("Failed to create session: {e}")),
    }
}

async fn handle_session_prompt(req: Request, state: &mut AcpServerState) -> Response {
    if !state.session_active {
        return Response::err(req.id, -32000, "No active session; call session/new first");
    }

    let text = match req
        .params
        .as_ref()
        .and_then(|p| p.get("text"))
        .and_then(Value::as_str)
    {
        Some(t) => t.to_string(),
        None => return Response::err(req.id, -32602, "Missing params.text"),
    };

    let ctx = match state.ctx.as_mut() {
        Some(c) => c,
        None => return Response::err(req.id, -32000, "Server not configured with a context"),
    };

    let abort = state.abort.clone();
    match run_prompt_turn(ctx, &text, abort).await {
        Ok(output) => Response::ok(
            req.id,
            json!({ "output": output, "stopReason": "end_turn" }),
        ),
        Err(e) => Response::err(req.id, -32000, format!("Prompt failed: {e}")),
    }
}

async fn run_prompt_turn(
    ctx: &mut RequestContext,
    text: &str,
    abort: AbortSignal,
) -> Result<String> {
    ctx.render_mode = RenderMode::Silent;
    let input = Input::from_str(ctx, text, None)?;
    ctx.before_chat_completion(&input)?;
    let client = input.create_client()?;
    let (output, tool_results) =
        call_chat_completions_streaming(&input, client.as_ref(), ctx, abort).await?;
    let app = Arc::clone(&ctx.app.config);

    ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;

    Ok(output)
}

fn handle_session_cancel(state: &mut AcpServerState) {
    state.abort.set_ctrlc();
}

async fn handle_session_load(req: Request, state: &mut AcpServerState) -> Response {
    if state.session_active {
        return Response::err(req.id, -32000, "Session already active");
    }

    let session_name = match req
        .params
        .as_ref()
        .and_then(|p| p.get("sessionId"))
        .and_then(Value::as_str)
    {
        Some(n) => n.to_string(),
        None => return Response::err(req.id, -32602, "Missing params.sessionId"),
    };

    let ctx = match state.ctx.as_mut() {
        Some(c) => c,
        None => return Response::err(req.id, -32000, "Server not configured with a context"),
    };

    let app = Arc::clone(&ctx.app.config);
    let abort = state.abort.clone();
    match ctx
        .use_session(app.as_ref(), Some(&session_name), abort)
        .await
    {
        Ok(_) => {
            state.session_active = true;
            ctx.render_mode = RenderMode::Silent;
            Response::ok(req.id, json!({ "sessionId": session_name }))
        }
        Err(e) => Response::err(req.id, -32000, format!("Failed to load session: {e}")),
    }
}

async fn emit<W: AsyncWrite + Unpin>(writer: &mut W, response: &Response) -> Result<()> {
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;

    Ok(())
}

async fn emit_notification<W: AsyncWrite + Unpin>(
    writer: &mut W,
    method: &str,
    params: Value,
) -> Result<()> {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&frame)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str;

    #[tokio::test]
    async fn all_stdout_is_valid_json_rpc() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"name":"test","version":"0.1.0"}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        for line in output.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let s = str::from_utf8(line).expect("non-UTF8 in ACP stdout");
            let _: Value = serde_json::from_str(s)
                .unwrap_or_else(|_| panic!("ACP stdout not valid JSON: {s}"));
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":2,"method":"nonexistent","params":{}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["id"], 2);
    }

    #[tokio::test]
    async fn invalid_json_returns_parse_error() {
        let input = "not json\n";
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["error"]["code"], PARSE_ERROR);
    }

    #[tokio::test]
    async fn notification_without_id_produces_no_output() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#,
            "\n",
        );
        let mut output = Vec::new();

        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"name":"test","version":"0.1.0"}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["name"], "coyote");
        assert!(v["result"]["version"].is_string());
    }

    #[tokio::test]
    async fn session_new_returns_session_id() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":10,"method":"session/new","params":{}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["id"], 10);
        assert_eq!(v["result"]["sessionId"], "default");
    }

    #[tokio::test]
    async fn session_new_twice_errors() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let mut lines = s.lines().filter(|l| !l.is_empty());

        let first: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let second: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert!(first["result"]["sessionId"].is_string());
        assert_eq!(second["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn session_prompt_without_session_errors() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"hello"}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn session_prompt_with_no_context_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"hello"}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let mut lines = s.lines().filter(|l| !l.is_empty());

        let first: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let second: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["result"]["sessionId"], "default");
        assert_eq!(second["id"], 2);
        assert!(
            second["error"].is_object(),
            "expected error response when no ctx"
        );
    }

    #[tokio::test]
    async fn session_cancel_notification_produces_no_output() {
        let input = concat!(r#"{"jsonrpc":"2.0","method":"session/cancel"}"#, "\n",);
        let mut output = Vec::new();

        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn session_cancel_request_returns_ok() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":99,"method":"session/cancel"}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["id"], 99);
        assert!(v["result"].is_object());
    }

    #[tokio::test]
    async fn session_load_missing_session_id_errors() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":5,"method":"session/load","params":{}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["id"], 5);
        assert_eq!(v["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn session_load_after_session_new_errors() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"abc"}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let mut lines = s.lines().filter(|l| !l.is_empty());

        let _first: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let second: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(second["id"], 2);
        assert_eq!(second["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn session_load_with_no_context_returns_error() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":6,"method":"session/load","params":{"sessionId":"my-session"}}"#,
            "\n",
        );
        let mut output = Vec::new();
        run_acp_server_on(input.as_bytes(), &mut output)
            .await
            .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["id"], 6);
        assert!(v["error"].is_object());
    }

    #[tokio::test]
    async fn emit_notification_produces_valid_json_rpc_frame() {
        let mut output = Vec::new();
        emit_notification(
            &mut output,
            "session/request_permission",
            json!({"action": "confirm", "question": "Proceed?"}),
        )
        .await
        .unwrap();

        let s = String::from_utf8(output).unwrap();
        let v: Value = serde_json::from_str(s.trim()).unwrap();

        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "session/request_permission");
        assert!(v["params"]["action"].is_string());
        assert!(!v.as_object().unwrap().contains_key("id"));
    }
}
