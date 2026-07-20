use super::oauth::{OAuthProvider, TokenRequestFormat};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

pub struct OpenAIOAuthProvider;

impl OAuthProvider for OpenAIOAuthProvider {
    fn provider_name(&self) -> &str {
        "openai"
    }

    fn client_id(&self) -> &str {
        "app_EMoamEEZ73f0CkXaXp7hrann"
    }

    fn authorize_url(&self) -> &str {
        "https://auth.openai.com/oauth/authorize"
    }

    fn token_url(&self) -> &str {
        "https://auth.openai.com/oauth/token"
    }

    fn redirect_uri(&self) -> &str {
        "http://localhost:1455/auth/callback"
    }

    fn scopes(&self) -> String {
        "openid profile email offline_access".to_string()
    }

    fn token_request_format(&self) -> TokenRequestFormat {
        TokenRequestFormat::FormUrlEncoded
    }

    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        vec![
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
        ]
    }

    fn fixed_redirect_uri(&self) -> Option<String> {
        Some("http://localhost:1455/auth/callback".to_string())
    }

    fn include_state_in_token_exchange(&self) -> bool {
        false
    }

    fn extract_account_id(&self, response: &Value) -> Option<String> {
        let id_token = response["id_token"].as_str().unwrap_or_default();
        let access_token = response["access_token"].as_str().unwrap_or_default();
        extract_account_id_from_jwt(id_token).or_else(|| extract_account_id_from_jwt(access_token))
    }
}

fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() < 2 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims["chatgpt_account_id"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| {
            claims["https://api.openai.com/auth"]["chatgpt_account_id"]
                .as_str()
                .map(|s| s.to_string())
        })
        .or_else(|| {
            claims["organizations"][0]["id"]
                .as_str()
                .map(|s| s.to_string())
        })
}
