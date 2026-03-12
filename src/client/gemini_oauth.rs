use super::oauth::{OAuthProvider, TokenRequestFormat};

pub struct GeminiOAuthProvider;

const GEMINI_CLIENT_ID: &str =
    "50826443741-upqcebrs4gctqht1f08ku46qlbirkdsj.apps.googleusercontent.com";
const GEMINI_CLIENT_SECRET: &str = "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";

impl OAuthProvider for GeminiOAuthProvider {
    fn provider_name(&self) -> &str {
        "gemini"
    }

    fn client_id(&self) -> &str {
        GEMINI_CLIENT_ID
    }

    fn authorize_url(&self) -> &str {
        "https://accounts.google.com/o/oauth2/v2/auth"
    }

    fn token_url(&self) -> &str {
        "https://oauth2.googleapis.com/token"
    }

    fn redirect_uri(&self) -> &str {
        ""
    }

    fn scopes(&self) -> &str {
        "https://www.googleapis.com/auth/generative-language.peruserquota https://www.googleapis.com/auth/generative-language.retriever https://www.googleapis.com/auth/userinfo.email"
    }

    fn client_secret(&self) -> Option<&str> {
        Some(GEMINI_CLIENT_SECRET)
    }

    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        vec![("access_type", "offline"), ("prompt", "consent")]
    }

    fn token_request_format(&self) -> TokenRequestFormat {
        TokenRequestFormat::FormUrlEncoded
    }

    fn uses_localhost_redirect(&self) -> bool {
        true
    }
}
