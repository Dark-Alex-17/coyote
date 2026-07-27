use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const METHOD_NOT_FOUND: i32 = -32601;
pub const PARSE_ERROR: i32 = -32700;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Ok { result: Value },
    Err { error: RpcError },
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            body: ResponseBody::Ok { result },
        }
    }

    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            body: ResponseBody::Err {
                error: RpcError {
                    code,
                    message: message.into(),
                },
            },
        }
    }
}
