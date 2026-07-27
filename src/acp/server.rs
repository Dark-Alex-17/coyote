use super::types::{METHOD_NOT_FOUND, PARSE_ERROR, Request, Response};
use anyhow::Result;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub async fn run_acp_server() -> Result<()> {
    run_acp_server_on(tokio::io::stdin(), tokio::io::stdout()).await
}

pub(crate) async fn run_acp_server_on<R, W>(reader: R, mut writer: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reader = BufReader::new(reader);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(response) = dispatch(&line) {
            emit(&mut writer, &response).await?;
        }
    }

    Ok(())
}

fn dispatch(raw: &str) -> Option<Response> {
    let req: Request = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(_) => return Some(Response::err(None, PARSE_ERROR, "Parse error")),
    };

    req.id.as_ref()?;

    Some(match req.method.as_str() {
        "initialize" => handle_initialize(req),
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

async fn emit<W: AsyncWrite + Unpin>(writer: &mut W, response: &Response) -> Result<()> {
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let s = std::str::from_utf8(line).expect("non-UTF8 in ACP stdout");
            let _: serde_json::Value = serde_json::from_str(s)
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
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
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
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
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
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["name"], "coyote");
        assert!(v["result"]["version"].is_string());
    }
}
