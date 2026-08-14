use crate::mcp::oauth::{force_refresh_mcp_token, load_or_refresh_mcp_token};
use http::{HeaderName, HeaderValue};
use log::debug;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

/// [`StreamableHttpClient`] wrapper that injects the OAuth bearer token for an
/// MCP server on every request instead of pinning it at spawn time, so tokens
/// refreshed mid-session take effect without reconnecting.
///
/// A caller-supplied `auth_header` always passes through untouched; only a
/// `None` header is filled from the stored token. When the wrapper injected
/// the token and a POST comes back 401, it forces a token refresh and retries
/// exactly once (see [`Self::post_with_retry`]).
#[derive(Clone)]
pub struct McpOAuthClient<C = reqwest::Client> {
    inner: C,
    server: Arc<str>,
}

impl<C> McpOAuthClient<C> {
    pub fn new(inner: C, server: &str) -> Self {
        Self {
            inner,
            server: Arc::from(server),
        }
    }
}

impl<C: StreamableHttpClient + Sync> McpOAuthClient<C> {
    /// Resolves the effective auth header. Caller-supplied values pass through
    /// untouched; `None` is filled from the stored token for this server.
    /// Returns the header plus whether the wrapper injected it. Errors with
    /// [`StreamableHttpError::AuthRequired`] (without contacting the server)
    /// when no usable token exists.
    async fn resolve_auth(
        &self,
        auth_header: Option<String>,
    ) -> Result<(Option<String>, bool), StreamableHttpError<C::Error>> {
        if auth_header.is_some() {
            return Ok((auth_header, false));
        }
        match load_or_refresh_mcp_token(&self.server).await.into_token() {
            Some(token) => Ok((Some(token), true)),
            None => Err(self.auth_required()),
        }
    }

    fn auth_required(&self) -> StreamableHttpError<C::Error> {
        StreamableHttpError::AuthRequired(AuthRequiredError::new(format!(
            "no valid OAuth token for MCP server '{server}'; \
             run `.mcp auth {server}` to re-authenticate",
            server = self.server
        )))
    }

    async fn post_with_retry<F, Fut>(
        &self,
        auth_header: Option<String>,
        mut post: F,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<C::Error>>
    where
        F: FnMut(Option<String>) -> Fut,
        Fut: Future<Output = Result<StreamableHttpPostResponse, StreamableHttpError<C::Error>>>,
    {
        let (auth, injected) = self.resolve_auth(auth_header).await?;
        let (original, rejected) = match (post(auth.clone()).await, auth) {
            (Err(err @ StreamableHttpError::AuthRequired(_)), Some(rejected)) if injected => {
                (err, rejected)
            }
            (result, _) => return result,
        };

        debug!(
            "MCP server '{}' rejected the injected token; forcing a refresh and retrying once",
            self.server
        );

        let Some(token) = force_refresh_mcp_token(&self.server, &rejected).await else {
            return Err(original);
        };

        match post(Some(token)).await {
            Err(StreamableHttpError::AuthRequired(_)) => {
                debug!(
                    "Retry after forced token refresh was rejected again by MCP server '{}'",
                    self.server
                );
                Err(original)
            }
            result => result,
        }
    }
}

impl<C: StreamableHttpClient + Sync> StreamableHttpClient for McpOAuthClient<C> {
    type Error = C::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_with_retry(auth_header, |auth| {
            self.inner.post_message(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                auth,
                custom_headers.clone(),
            )
        })
        .await
    }

    /// Overridden rather than left to the trait default: the default impl
    /// delegates to [`Self::post_message`], silently dropping the
    /// transport-wide SSE event size limit. Delegating to the inner client's
    /// size-enforcing variant keeps the limit applied at the raw byte layer.
    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_with_retry(auth_header, |auth| {
            self.inner.post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                auth,
                custom_headers.clone(),
                max_sse_event_size,
            )
        })
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let (auth, _) = self.resolve_auth(auth_header).await?;
        self.inner
            .delete_session(uri, session_id, auth, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        let (auth, _) = self.resolve_auth(auth_header).await?;
        self.inner
            .get_stream(uri, session_id, last_event_id, auth, custom_headers)
            .await
    }

    /// Overridden for the same reason as
    /// [`Self::post_message_with_max_sse_event_size`]: the trait default
    /// bypasses SSE event size enforcement.
    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        let (auth, _) = self.resolve_auth(auth_header).await?;
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths;
    use crate::mcp::oauth::test_support::with_temp_cache;
    use futures_util::StreamExt;
    use parking_lot::Mutex;
    use serial_test::serial;
    use std::convert::Infallible;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FRESH: i64 = 9999999999;

    type Calls = Arc<Mutex<Vec<(&'static str, Option<String>)>>>;

    type AfterFirstCallAction = Option<Box<dyn FnOnce() + Send + 'static>>;

    /// Inner client that records `(method, auth_header)` per call, rejects the
    /// first `reject_times` POSTs/streams with `AuthRequired` (then the next
    /// `transport_error_times` with a non-auth error), and runs an optional
    /// side effect after the first call (to mutate token files between the
    /// initial attempt and the retry).
    #[derive(Clone, Default)]
    struct FakeInner {
        calls: Calls,
        reject_times: Arc<AtomicUsize>,
        transport_error_times: Arc<AtomicUsize>,
        after_first_call: Arc<Mutex<AfterFirstCallAction>>,
    }

    impl FakeInner {
        fn record(
            &self,
            method: &'static str,
            auth: Option<String>,
        ) -> Result<(), StreamableHttpError<Infallible>> {
            self.calls.lock().push((method, auth));
            if let Some(f) = self.after_first_call.lock().take() {
                f();
            }
            if self.reject_times.load(Ordering::SeqCst) > 0 {
                self.reject_times.fetch_sub(1, Ordering::SeqCst);
                return Err(Self::rejection());
            }
            if self.transport_error_times.load(Ordering::SeqCst) > 0 {
                self.transport_error_times.fetch_sub(1, Ordering::SeqCst);
                return Err(StreamableHttpError::UnexpectedServerResponse(
                    "connection reset".into(),
                ));
            }
            Ok(())
        }

        fn rejection() -> StreamableHttpError<Infallible> {
            StreamableHttpError::AuthRequired(AuthRequiredError::new(
                "Bearer error=\"invalid_token\"".to_string(),
            ))
        }
    }

    impl StreamableHttpClient for FakeInner {
        type Error = Infallible;

        async fn post_message(
            &self,
            _uri: Arc<str>,
            _message: ClientJsonRpcMessage,
            _session_id: Option<Arc<str>>,
            auth_header: Option<String>,
            _custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
            self.record("post_message", auth_header)?;
            Ok(StreamableHttpPostResponse::Accepted)
        }

        async fn post_message_with_max_sse_event_size(
            &self,
            _uri: Arc<str>,
            _message: ClientJsonRpcMessage,
            _session_id: Option<Arc<str>>,
            auth_header: Option<String>,
            _custom_headers: HashMap<HeaderName, HeaderValue>,
            _max_sse_event_size: usize,
        ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
            self.record("post_message_with_max_sse_event_size", auth_header)?;
            Ok(StreamableHttpPostResponse::Accepted)
        }

        async fn delete_session(
            &self,
            _uri: Arc<str>,
            _session_id: Arc<str>,
            auth_header: Option<String>,
            _custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<(), StreamableHttpError<Self::Error>> {
            self.record("delete_session", auth_header)?;
            Ok(())
        }

        async fn get_stream(
            &self,
            _uri: Arc<str>,
            _session_id: Option<Arc<str>>,
            _last_event_id: Option<String>,
            auth_header: Option<String>,
            _custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
            self.record("get_stream", auth_header)?;
            Ok(futures_util::stream::empty().boxed())
        }

        async fn get_stream_with_max_sse_event_size(
            &self,
            _uri: Arc<str>,
            _session_id: Option<Arc<str>>,
            _last_event_id: Option<String>,
            auth_header: Option<String>,
            _custom_headers: HashMap<HeaderName, HeaderValue>,
            _max_sse_event_size: usize,
        ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
            self.record("get_stream_with_max_sse_event_size", auth_header)?;
            Ok(futures_util::stream::empty().boxed())
        }
    }

    fn write_token_file(server: &str, access_token: &str, expires_at: i64) {
        fs::create_dir_all(paths::oauth_tokens_dir()).unwrap();
        fs::write(
            paths::token_file(&format!("mcp_{server}")),
            format!(
                r#"{{"access_token":"{access_token}","refresh_token":"r","expires_at":{expires_at}}}"#
            ),
        )
        .unwrap();
    }

    fn ping() -> ClientJsonRpcMessage {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping"
        }))
        .unwrap()
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn post(
        client: &McpOAuthClient<FakeInner>,
        auth_header: Option<String>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Infallible>> {
        rt().block_on(client.post_message(
            Arc::from("http://mcp.test/mcp"),
            ping(),
            None,
            auth_header,
            HashMap::new(),
        ))
    }

    #[test]
    #[serial]
    fn injects_token_from_disk_when_auth_header_none() {
        with_temp_cache(|| {
            write_token_file("wrapper-inject", "tok-live", FRESH);
            let inner = FakeInner::default();
            let client = McpOAuthClient::new(inner.clone(), "wrapper-inject");

            let result = post(&client, None);

            assert!(result.is_ok());
            assert_eq!(
                *inner.calls.lock(),
                vec![("post_message", Some("tok-live".to_string()))]
            );
        });
    }

    #[test]
    #[serial]
    fn caller_supplied_auth_header_passes_through() {
        with_temp_cache(|| {
            write_token_file("wrapper-passthrough", "tok-disk", FRESH);
            let inner = FakeInner::default();
            let client = McpOAuthClient::new(inner.clone(), "wrapper-passthrough");

            let result = post(&client, Some("caller-tok".to_string()));

            assert!(result.is_ok());
            assert_eq!(
                *inner.calls.lock(),
                vec![("post_message", Some("caller-tok".to_string()))]
            );
        });
    }

    #[test]
    #[serial]
    fn caller_supplied_header_rejection_propagates_without_refresh() {
        with_temp_cache(|| {
            // A fresh, different token sits on disk: if the injected guard
            // were dropped, the wrapper would refresh and retry with it.
            write_token_file("wrapper-caller-401", "tok-disk", FRESH);
            let inner = FakeInner::default();
            inner.reject_times.store(1, Ordering::SeqCst);
            let client = McpOAuthClient::new(inner.clone(), "wrapper-caller-401");

            let result = post(&client, Some("caller-tok".to_string()));

            assert!(matches!(result, Err(StreamableHttpError::AuthRequired(_))));
            assert_eq!(
                *inner.calls.lock(),
                vec![("post_message", Some("caller-tok".to_string()))]
            );
        });
    }

    #[test]
    #[serial]
    fn missing_token_returns_auth_required_without_calling_inner() {
        with_temp_cache(|| {
            let inner = FakeInner::default();
            let client = McpOAuthClient::new(inner.clone(), "wrapper-no-token");

            let result = post(&client, None);

            assert!(matches!(result, Err(StreamableHttpError::AuthRequired(_))));
            assert!(inner.calls.lock().is_empty());
        });
    }

    #[test]
    #[serial]
    fn rejected_token_forces_refresh_and_retries_once() {
        with_temp_cache(|| {
            write_token_file("wrapper-retry", "tok-a", FRESH);
            let inner = FakeInner::default();
            inner.reject_times.store(1, Ordering::SeqCst);
            // Simulate a concurrent refresh landing between the rejection and
            // the forced refresh: the retry must carry the new token.
            *inner.after_first_call.lock() = Some(Box::new(|| {
                write_token_file("wrapper-retry", "tok-b", FRESH);
            }));
            let client = McpOAuthClient::new(inner.clone(), "wrapper-retry");

            let result = post(&client, None);

            assert!(result.is_ok());
            assert_eq!(
                *inner.calls.lock(),
                vec![
                    ("post_message", Some("tok-a".to_string())),
                    ("post_message", Some("tok-b".to_string())),
                ]
            );
        });
    }

    #[test]
    #[serial]
    fn failed_force_refresh_propagates_original_error_after_one_call() {
        with_temp_cache(|| {
            write_token_file("wrapper-refresh-fail", "tok-a", FRESH);
            let inner = FakeInner::default();
            inner.reject_times.store(1, Ordering::SeqCst);
            // Token file gone by refresh time: force refresh yields nothing.
            *inner.after_first_call.lock() = Some(Box::new(|| {
                fs::remove_file(paths::token_file("mcp_wrapper-refresh-fail")).unwrap();
            }));
            let client = McpOAuthClient::new(inner.clone(), "wrapper-refresh-fail");

            let result = post(&client, None);

            assert!(matches!(result, Err(StreamableHttpError::AuthRequired(_))));
            assert_eq!(inner.calls.lock().len(), 1);
        });
    }

    #[test]
    #[serial]
    fn second_rejection_after_retry_propagates_original_error() {
        with_temp_cache(|| {
            write_token_file("wrapper-double-401", "tok-a", FRESH);
            let inner = FakeInner::default();
            inner.reject_times.store(2, Ordering::SeqCst);
            // A changed token appears before the forced refresh, so the retry
            // actually runs (an unchanged token would trigger a real refresh
            // attempt, which fails without a cached registration).
            *inner.after_first_call.lock() = Some(Box::new(|| {
                write_token_file("wrapper-double-401", "tok-b", FRESH);
            }));
            let client = McpOAuthClient::new(inner.clone(), "wrapper-double-401");

            let result = post(&client, None);

            assert!(matches!(result, Err(StreamableHttpError::AuthRequired(_))));
            assert_eq!(inner.calls.lock().len(), 2);
        });
    }

    #[test]
    #[serial]
    fn non_auth_retry_error_propagates_as_is() {
        with_temp_cache(|| {
            write_token_file("wrapper-retry-transport", "tok-a", FRESH);
            let inner = FakeInner::default();
            inner.reject_times.store(1, Ordering::SeqCst);
            inner.transport_error_times.store(1, Ordering::SeqCst);
            *inner.after_first_call.lock() = Some(Box::new(|| {
                write_token_file("wrapper-retry-transport", "tok-b", FRESH);
            }));
            let client = McpOAuthClient::new(inner.clone(), "wrapper-retry-transport");

            let result = post(&client, None);

            assert!(matches!(
                result,
                Err(StreamableHttpError::UnexpectedServerResponse(_))
            ));
            assert_eq!(inner.calls.lock().len(), 2);
        });
    }

    #[test]
    #[serial]
    fn sized_post_delegates_to_inner_sized_variant() {
        with_temp_cache(|| {
            write_token_file("wrapper-sized-post", "tok-live", FRESH);
            let inner = FakeInner::default();
            let client = McpOAuthClient::new(inner.clone(), "wrapper-sized-post");

            let result = rt().block_on(client.post_message_with_max_sse_event_size(
                Arc::from("http://mcp.test/mcp"),
                ping(),
                None,
                None,
                HashMap::new(),
                4096,
            ));

            assert!(result.is_ok());
            assert_eq!(
                *inner.calls.lock(),
                vec![(
                    "post_message_with_max_sse_event_size",
                    Some("tok-live".to_string())
                )]
            );
        });
    }

    #[test]
    #[serial]
    fn sized_get_stream_delegates_to_inner_sized_variant() {
        with_temp_cache(|| {
            write_token_file("wrapper-sized-get", "tok-live", FRESH);
            let inner = FakeInner::default();
            let client = McpOAuthClient::new(inner.clone(), "wrapper-sized-get");

            let result = rt().block_on(client.get_stream_with_max_sse_event_size(
                Arc::from("http://mcp.test/mcp"),
                None,
                None,
                None,
                HashMap::new(),
                4096,
            ));

            assert!(result.is_ok());
            assert_eq!(
                *inner.calls.lock(),
                vec![(
                    "get_stream_with_max_sse_event_size",
                    Some("tok-live".to_string())
                )]
            );
        });
    }

    #[test]
    #[serial]
    fn get_stream_does_not_retry_on_rejection() {
        with_temp_cache(|| {
            write_token_file("wrapper-get-401", "tok-live", FRESH);
            let inner = FakeInner::default();
            inner.reject_times.store(1, Ordering::SeqCst);
            let client = McpOAuthClient::new(inner.clone(), "wrapper-get-401");

            let result = rt().block_on(client.get_stream(
                Arc::from("http://mcp.test/mcp"),
                None,
                None,
                None,
                HashMap::new(),
            ));

            assert!(matches!(result, Err(StreamableHttpError::AuthRequired(_))));
            assert_eq!(inner.calls.lock().len(), 1);
        });
    }

    #[test]
    #[serial]
    fn delete_session_injects_token() {
        with_temp_cache(|| {
            write_token_file("wrapper-delete", "tok-live", FRESH);
            let inner = FakeInner::default();
            let client = McpOAuthClient::new(inner.clone(), "wrapper-delete");

            let result = rt().block_on(client.delete_session(
                Arc::from("http://mcp.test/mcp"),
                Arc::from("session-1"),
                None,
                HashMap::new(),
            ));

            assert!(result.is_ok());
            assert_eq!(
                *inner.calls.lock(),
                vec![("delete_session", Some("tok-live".to_string()))]
            );
        });
    }

    #[test]
    #[serial]
    fn auth_required_error_contains_no_token_material() {
        with_temp_cache(|| {
            write_token_file("wrapper-redact", "stale-secret-token", 0);
            let inner = FakeInner::default();
            let client = McpOAuthClient::new(inner.clone(), "wrapper-redact");

            let err = post(&client, None).unwrap_err();

            let display = format!("{err}");
            let debug = format!("{err:?}");
            assert!(!display.contains("stale-secret-token"));
            assert!(!debug.contains("stale-secret-token"));
            assert!(inner.calls.lock().is_empty());
        });
    }
}
