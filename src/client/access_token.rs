use anyhow::{Result, anyhow};
use chrono::Utc;
use indexmap::IndexMap;
use parking_lot::RwLock;
use std::sync::LazyLock;

type AccessTokenEntry = (String, i64, Option<String>);

static ACCESS_TOKENS: LazyLock<RwLock<IndexMap<String, AccessTokenEntry>>> =
    LazyLock::new(|| RwLock::new(IndexMap::new()));

pub fn get_access_token(client_name: &str) -> Result<String> {
    ACCESS_TOKENS
        .read()
        .get(client_name)
        .map(|(token, _, _)| token.clone())
        .ok_or_else(|| anyhow!("Invalid access token"))
}

pub fn get_access_token_account_id(client_name: &str) -> Option<String> {
    ACCESS_TOKENS
        .read()
        .get(client_name)
        .and_then(|(_, _, account_id)| account_id.clone())
}

pub fn is_valid_access_token(client_name: &str) -> bool {
    let access_tokens = ACCESS_TOKENS.read();
    let (token, expires_at, _) = match access_tokens.get(client_name) {
        Some(v) => v,
        None => return false,
    };
    !token.is_empty() && Utc::now().timestamp() < *expires_at
}

pub fn set_access_token(
    client_name: &str,
    token: String,
    expires_at: i64,
    account_id: Option<String>,
) {
    let mut access_tokens = ACCESS_TOKENS.write();
    let entry = access_tokens.entry(client_name.to_string()).or_default();
    entry.0 = token;
    entry.1 = expires_at;
    entry.2 = account_id;
}
