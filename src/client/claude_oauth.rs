use super::oauth::OAuthProvider;

pub const BETA_HEADER: &str = "oauth-2025-04-20";

pub struct ClaudeOAuthProvider;

impl OAuthProvider for ClaudeOAuthProvider {
    fn provider_name(&self) -> &str {
        "claude"
    }

    fn client_id(&self) -> &str {
        "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
    }

    fn authorize_url(&self) -> &str {
        "https://claude.ai/oauth/authorize"
    }

    fn token_url(&self) -> &str {
        "https://console.anthropic.com/v1/oauth/token"
    }

    fn redirect_uri(&self) -> &str {
        "https://console.anthropic.com/oauth/code/callback"
    }

    fn scopes(&self) -> &str {
        "org:create_api_key user:profile user:inference"
    }

    fn extra_token_headers(&self) -> Vec<(&str, &str)> {
        vec![("anthropic-beta", BETA_HEADER)]
    }

    fn extra_request_headers(&self) -> Vec<(&str, &str)> {
        vec![("anthropic-beta", BETA_HEADER)]
    }
}
