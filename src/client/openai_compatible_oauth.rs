use url::Url;

use super::oauth::{OAuthConfig, OAuthFlow, OAuthProvider, TokenRequestFormat};

pub struct OpenAICompatibleOAuthProvider {
    pub config: OAuthConfig,
    pub client_name: String,
}

fn is_loopback_uri(uri: &str) -> bool {
    Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1"))
}

impl OAuthProvider for OpenAICompatibleOAuthProvider {
    fn provider_name(&self) -> &str {
        &self.client_name
    }

    fn client_id(&self) -> &str {
        &self.config.client_id
    }

    fn authorize_url(&self) -> &str {
        self.config.authorize_url.as_deref().unwrap_or("")
    }

    fn token_url(&self) -> &str {
        &self.config.token_url
    }

    fn redirect_uri(&self) -> &str {
        self.config.redirect_uri.as_deref().unwrap_or("")
    }

    fn scopes(&self) -> String {
        self.config.scopes.join(" ")
    }

    fn client_secret(&self) -> Option<&str> {
        self.config.client_secret.as_deref()
    }

    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        self.config
            .extra_authorize_params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn token_request_format(&self) -> TokenRequestFormat {
        self.config
            .token_request_format
            .unwrap_or(TokenRequestFormat::FormUrlEncoded)
    }

    fn uses_localhost_redirect(&self) -> bool {
        self.config.redirect_uri.is_none() && self.config.redirect_port.is_none()
    }

    fn extra_token_headers(&self) -> Vec<(&str, &str)> {
        self.config
            .extra_token_headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn extra_request_headers(&self) -> Vec<(&str, &str)> {
        self.config
            .extra_request_headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn fixed_redirect_uri(&self) -> Option<String> {
        if let Some(uri) = &self.config.redirect_uri {
            return if is_loopback_uri(uri) {
                Some(uri.clone())
            } else {
                None
            };
        }
        if let Some(port) = self.config.redirect_port {
            return Some(format!("http://127.0.0.1:{port}/callback"));
        }
        None
    }

    fn include_state_in_token_exchange(&self) -> bool {
        self.config.include_state_in_token_exchange
    }

    fn flow(&self) -> OAuthFlow {
        self.config.flow
    }

    fn echo_pkce_in_token_exchange(&self) -> bool {
        self.config.echo_pkce_in_token_exchange
    }

    fn device_authorization_url(&self) -> Option<&str> {
        self.config.device_authorization_url.as_deref()
    }

    fn use_pkce_in_device_flow(&self) -> bool {
        self.config.use_pkce_in_device_flow
    }
}
