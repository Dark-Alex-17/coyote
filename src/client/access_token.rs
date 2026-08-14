use anyhow::{Result, anyhow};
use chrono::Utc;
use indexmap::IndexMap;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::LazyLock;

type AccessTokenEntry = (String, i64, Option<String>);

static ACCESS_TOKENS: LazyLock<RwLock<IndexMap<String, AccessTokenEntry>>> =
    LazyLock::new(|| RwLock::new(IndexMap::new()));

/// Tokens a provider rejected (401) despite being locally unexpired.
/// Maps client name → the exact rejected token so a concurrently-refreshed
/// different token is never distrusted by mistake.
static REJECTED_TOKENS: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

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
    !token.is_empty() && Utc::now().timestamp() < *expires_at && !is_rejected(client_name, token)
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

/// Compare-and-invalidate a provider-rejected token.
///
/// Only if the currently-cached token EQUALS `rejected` is the cache entry
/// removed and the rejection marker recorded; a concurrently-refreshed
/// different token is left untouched and no marker is set.
///
/// Returns true if a cache entry existed for this client at all (whether or
/// not it matched `rejected`) — i.e. the client is token-authed and a retry
/// after refresh is worthwhile. Returns false when there is no entry
/// (API-key clients).
#[allow(dead_code)] // Called by the 401-retry path once it lands.
pub fn distrust_access_token(client_name: &str, rejected: &str) -> bool {
    let mut access_tokens = ACCESS_TOKENS.write();
    let (token, _, _) = match access_tokens.get(client_name) {
        Some(v) => v,
        None => return false,
    };
    if token == rejected {
        access_tokens.shift_remove(client_name);
        REJECTED_TOKENS
            .write()
            .insert(client_name.to_string(), rejected.to_string());
    }
    true
}

pub fn is_rejected(client_name: &str, token: &str) -> bool {
    REJECTED_TOKENS
        .read()
        .get(client_name)
        .is_some_and(|rejected| rejected == token)
}

pub fn clear_rejected(client_name: &str) {
    REJECTED_TOKENS.write().remove(client_name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distrust_removes_matching_token_and_sets_marker() {
        let client = "distrust-match-test";
        set_access_token(client, "at-1".into(), Utc::now().timestamp() + 3600, None);

        assert!(distrust_access_token(client, "at-1"));

        assert!(get_access_token(client).is_err(), "cache entry not removed");
        assert!(is_rejected(client, "at-1"), "marker not set");
    }

    #[test]
    fn distrust_keeps_differing_token_and_skips_marker() {
        let client = "distrust-differ-test";
        set_access_token(client, "at-new".into(), Utc::now().timestamp() + 3600, None);

        assert!(distrust_access_token(client, "at-old"));

        assert_eq!(get_access_token(client).unwrap(), "at-new");
        assert!(!is_rejected(client, "at-old"), "marker set for stale token");
    }

    #[test]
    fn distrust_returns_false_without_cache_entry() {
        let client = "distrust-missing-test";

        assert!(!distrust_access_token(client, "at-1"));
        assert!(!is_rejected(client, "at-1"));
    }

    #[test]
    fn is_valid_access_token_false_for_rejected_token() {
        let client = "rejected-valid-test";
        set_access_token(client, "at-1".into(), Utc::now().timestamp() + 3600, None);
        assert!(is_valid_access_token(client));

        distrust_access_token(client, "at-1");
        // A concurrent in-flight prepare re-caches the rejected file token
        // between mark and refresh; it must still be treated as invalid.
        set_access_token(client, "at-1".into(), Utc::now().timestamp() + 3600, None);

        assert!(!is_valid_access_token(client));
    }

    #[test]
    fn clear_rejected_clears_marker_and_clients_are_isolated() {
        let client_a = "rejected-isolation-a";
        let client_b = "rejected-isolation-b";
        set_access_token(client_a, "at-1".into(), Utc::now().timestamp() + 3600, None);
        distrust_access_token(client_a, "at-1");

        assert!(is_rejected(client_a, "at-1"));
        assert!(
            !is_rejected(client_b, "at-1"),
            "marker leaked across clients"
        );

        clear_rejected(client_b);
        assert!(
            is_rejected(client_a, "at-1"),
            "wrong client's marker cleared"
        );

        clear_rejected(client_a);
        assert!(!is_rejected(client_a, "at-1"));
    }
}
