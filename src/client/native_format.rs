use super::structured_output::SchemaDialect;
use super::{ApiStatusError, Client};
use crate::config::{Input, RequestContext};
use anyhow::Result;
use log::warn;
use std::future::Future;
use std::pin::Pin;

/// Native structured-output bookkeeping for one call sequence (see
/// `RequestContext::native_format` for who resets it). A rejection is sticky:
/// once the provider refuses the format, later calls in the sequence go out
/// without it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct NativeFormatState {
    pub rejected: bool,
    pub last_dialect: Option<SchemaDialect>,
}

/// Sends `input` with its native format when `client` has a dialect for it
/// and the sequence has not seen a rejection. A 400 or an undecodable
/// response is taken as the provider refusing the format: the call is
/// repeated once without it, on the same client. Every other error propagates
/// after the first call.
pub(crate) async fn with_native_format_fallback<'a, T>(
    input: &'a Input,
    client: &'a dyn Client,
    ctx: &'a mut RequestContext,
    mut call: impl for<'b> FnMut(
        &'b Input,
        &'b dyn Client,
        &'b mut RequestContext,
    ) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'b>>,
) -> Result<T> {
    let dialect = if ctx.native_format.rejected {
        None
    } else {
        input.native_structured_output(client)
    };
    let Some(dialect) = dialect else {
        let result = if input.role().structured_output().is_some() {
            let plain = input.without_structured_output();
            call(&plain, client, ctx).await
        } else {
            call(input, client, ctx).await
        };
        ctx.native_format.last_dialect = None;
        return result;
    };

    let err = match call(input, client, ctx).await {
        Ok(out) => {
            ctx.native_format.last_dialect = Some(dialect);
            return Ok(out);
        }
        Err(err) => err,
    };
    let Some(reason) = format_rejection_reason(&err) else {
        return Err(err);
    };
    warn!("provider rejected the native structured-output format ({reason}); retrying without it");
    ctx.native_format.rejected = true;
    let plain = input.without_structured_output();
    let result = call(&plain, client, ctx).await;
    ctx.native_format.last_dialect = None;
    result
}

fn format_rejection_reason(err: &anyhow::Error) -> Option<String> {
    err.chain().find_map(|cause| {
        if let Some(api) = cause.downcast_ref::<ApiStatusError>() {
            (api.status == 400).then(|| api.message.clone())
        } else {
            cause
                .downcast_ref::<reqwest::Error>()
                .filter(|e| e.is_decode())
                .map(|_| format!("{err:#}"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::structured_output::StructuredOutputSpec;
    use crate::client::{ClientConfig, Model, ModelData, init_client};
    use crate::config::{AppConfig, AppState, Role, WorkingMode};
    use crate::graph::llm::CompletionFuture;
    use crate::graph::llm::test_support::boxed_completion;
    use crate::testing::{install_log_collector, warn_snapshot};
    use serde_json::{Value, json};
    use std::sync::Arc;

    fn make_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    fn format_call<F>(f: F) -> F
    where
        F: for<'b> FnMut(&'b Input, &'b dyn Client, &'b mut RequestContext) -> CompletionFuture<'b>
            + Send,
    {
        f
    }

    /// An OpenAI client whose model offers the native format exactly when
    /// `dialect` is set; only `None` and `OpenAiStrict` are reachable.
    fn client_for(dialect: Option<SchemaDialect>) -> Box<dyn Client> {
        let app_config = Arc::new(AppConfig {
            clients: vec![ClientConfig::OpenAIConfig(Default::default())],
            ..AppConfig::default()
        });
        let mut data = ModelData::new("structured");
        data.supports_structured_output = dialect.is_some();
        init_client(&app_config, Model::from_config("openai", &[data]).remove(0)).unwrap()
    }

    fn answer_spec() -> StructuredOutputSpec {
        StructuredOutputSpec {
            name: "answer".to_string(),
            schema: json!({"type": "object"}),
        }
    }

    fn input_with_spec(ctx: &RequestContext) -> Input {
        let mut role = Role::new("r", "");
        role.set_structured_output(Some(answer_spec()));
        Input::from_str(ctx, "hi", Some(role)).unwrap()
    }

    fn api_error(status: u16, message: &str) -> anyhow::Error {
        anyhow::Error::new(ApiStatusError {
            status,
            message: message.to_string(),
        })
        .context("Failed to call chat-completions api")
    }

    /// Runs the wrapper over a fake that fails its first call with
    /// `first_err` (when given) and answers "ok" otherwise; returns the
    /// result and the spec each call was sent with.
    async fn run_wrapper(
        dialect: Option<SchemaDialect>,
        input: &Input,
        ctx: &mut RequestContext,
        first_err: Option<anyhow::Error>,
    ) -> (Result<String>, Vec<Option<StructuredOutputSpec>>) {
        let client = client_for(dialect);
        let mut first_err = first_err;
        let mut seen = Vec::new();
        let result = with_native_format_fallback(
            input,
            client.as_ref(),
            ctx,
            format_call(|input, _client, _ctx| {
                seen.push(input.role().structured_output().cloned());
                let outcome = match first_err.take() {
                    Some(err) => Err(err),
                    None => Ok(("ok".to_string(), vec![])),
                };
                boxed_completion(async move { outcome })
            }),
        )
        .await
        .map(|(output, _)| output);
        (result, seen)
    }

    #[tokio::test]
    async fn native_format_fallback_without_a_dialect_calls_once_without_the_format() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(None, &input, &mut ctx, None).await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![None]);
        assert_eq!(ctx.native_format, NativeFormatState::default());
    }

    #[tokio::test]
    async fn native_format_fallback_records_the_dialect_on_success() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) =
            run_wrapper(Some(SchemaDialect::OpenAiStrict), &input, &mut ctx, None).await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![Some(answer_spec())]);
        assert_eq!(
            ctx.native_format.last_dialect,
            Some(SchemaDialect::OpenAiStrict)
        );
        assert!(!ctx.native_format.rejected);
    }

    #[tokio::test]
    async fn native_format_fallback_retries_once_without_the_format_on_a_400() {
        install_log_collector();
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);
        let marker = "schema keyword unsupported 7f3a";

        let (result, seen) = run_wrapper(
            Some(SchemaDialect::OpenAiStrict),
            &input,
            &mut ctx,
            Some(api_error(400, marker)),
        )
        .await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![Some(answer_spec()), None]);
        assert!(ctx.native_format.rejected);
        assert_eq!(ctx.native_format.last_dialect, None);
        let expected = format!(
            "provider rejected the native structured-output format ({marker}); retrying without it"
        );
        assert!(
            warn_snapshot().iter().any(|w| w == &expected),
            "missing warning: {expected}"
        );
    }

    #[tokio::test]
    async fn native_format_rejection_sticks_until_the_state_is_reset() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);
        let dialect = Some(SchemaDialect::OpenAiStrict);
        run_wrapper(dialect, &input, &mut ctx, Some(api_error(400, "rejected")))
            .await
            .0
            .unwrap();

        let (_, sticky) = run_wrapper(dialect, &input, &mut ctx, None).await;
        ctx.native_format = Default::default();
        let (_, fresh) = run_wrapper(dialect, &input, &mut ctx, None).await;

        assert_eq!(sticky, vec![None]);
        assert_eq!(fresh, vec![Some(answer_spec())]);
    }

    #[tokio::test]
    async fn native_format_fallback_propagates_other_statuses_after_one_call() {
        for status in [500, 429] {
            let mut ctx = make_ctx();
            let input = input_with_spec(&ctx);

            let (result, seen) = run_wrapper(
                Some(SchemaDialect::OpenAiStrict),
                &input,
                &mut ctx,
                Some(api_error(status, "busy")),
            )
            .await;

            let err = result.expect_err("a non-400 status must propagate");
            assert_eq!(
                err.chain()
                    .find_map(|c| c.downcast_ref::<ApiStatusError>())
                    .map(|e| e.status),
                Some(status)
            );
            assert_eq!(seen, vec![Some(answer_spec())], "status {status}");
            assert!(!ctx.native_format.rejected);
        }
    }

    #[tokio::test]
    async fn native_format_fallback_retries_after_a_decode_error() {
        let decode = decode_error().await;
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(
            Some(SchemaDialect::OpenAiStrict),
            &input,
            &mut ctx,
            Some(anyhow::Error::new(decode).context("Failed to call chat-completions api")),
        )
        .await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(seen, vec![Some(answer_spec()), None]);
        assert!(ctx.native_format.rejected);
    }

    async fn decode_error() -> reqwest::Error {
        let decode = reqwest::Response::from(http::Response::new("not json"))
            .json::<Value>()
            .await
            .expect_err("a non-JSON body must fail to decode");
        assert!(decode.is_decode());
        decode
    }

    fn api_status(err: &anyhow::Error) -> Option<(u16, String)> {
        err.chain()
            .find_map(|c| c.downcast_ref::<ApiStatusError>())
            .map(|e| (e.status, e.message.clone()))
    }

    #[tokio::test]
    async fn inactive_without_dialect_propagates_400_after_one_call() {
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) =
            run_wrapper(None, &input, &mut ctx, Some(api_error(400, "bad request"))).await;

        let err = result.expect_err("an inactive wrapper must not retry");
        assert_eq!(api_status(&err), Some((400, "bad request".to_string())));
        assert_eq!(seen, vec![None]);
        assert!(!ctx.native_format.rejected);
    }

    #[tokio::test]
    async fn inactive_when_rejected_propagates_400_after_one_call() {
        let mut ctx = make_ctx();
        ctx.native_format.rejected = true;
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(
            Some(SchemaDialect::OpenAiStrict),
            &input,
            &mut ctx,
            Some(api_error(400, "bad request")),
        )
        .await;

        let err = result.expect_err("an inactive wrapper must not retry");
        assert_eq!(api_status(&err), Some((400, "bad request".to_string())));
        assert_eq!(seen, vec![None]);
        assert!(ctx.native_format.rejected);
    }

    #[tokio::test]
    async fn inactive_without_dialect_propagates_decode_error_after_one_call() {
        let decode = decode_error().await;
        let mut ctx = make_ctx();
        let input = input_with_spec(&ctx);

        let (result, seen) = run_wrapper(
            None,
            &input,
            &mut ctx,
            Some(anyhow::Error::new(decode).context("Failed to call chat-completions api")),
        )
        .await;

        let err = result.expect_err("an inactive wrapper must not retry");
        assert!(
            err.chain()
                .filter_map(|c| c.downcast_ref::<reqwest::Error>())
                .any(reqwest::Error::is_decode),
            "decode error missing from the chain: {err:#}"
        );
        assert_eq!(seen, vec![None]);
        assert!(!ctx.native_format.rejected);
    }
}
