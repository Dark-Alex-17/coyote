use anyhow::{Context, Result, anyhow};
use eventsource_stream::{EventStream, Eventsource};
use fmt::{Display, Formatter};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use mpsc::error::SendError;
use mpsc::{OwnedPermit, Receiver, Sender, channel};
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use tokio::sync::mpsc;
use tokio::time::Duration;
use url::Url;

type SseEventStream = EventStream<BoxStream<'static, reqwest::Result<bytes::Bytes>>>;

const CHANNEL_BUF: usize = 64;

pub struct LegacySseTransport {
    tx: Sender<ClientJsonRpcMessage>,
    rx: Receiver<ServerJsonRpcMessage>,
}

impl LegacySseTransport {
    pub async fn connect(sse_url: &str, headers: Option<&HashMap<String, String>>) -> Result<Self> {
        let base_url =
            Url::parse(sse_url).with_context(|| format!("Invalid SSE URL: {sse_url}"))?;

        let mut client_builder = Client::builder();
        let mut header_map = HeaderMap::new();
        if let Some(hdrs) = headers {
            for (k, v) in hdrs {
                let name = k
                    .parse::<HeaderName>()
                    .with_context(|| format!("Invalid header name: {k}"))?;
                let value = v
                    .parse::<HeaderValue>()
                    .with_context(|| format!("Invalid header value for {k}"))?;
                header_map.insert(name, value);
            }
            client_builder = client_builder.default_headers(header_map);
        }
        let client = client_builder
            .build()
            .context("Failed to build HTTP client")?;

        let response = client
            .get(sse_url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .context("Failed to open SSE connection")?
            .error_for_status()
            .context("SSE server returned an error status")?;
        let mut es: SseEventStream = response.bytes_stream().boxed().eventsource();

        let post_endpoint = wait_for_endpoint_event(&mut es, &base_url).await?;

        let (outgoing_tx, outgoing_rx) = channel::<ClientJsonRpcMessage>(CHANNEL_BUF);
        let (incoming_tx, incoming_rx) = channel::<ServerJsonRpcMessage>(CHANNEL_BUF);

        tokio::spawn(sse_reader_task(es, incoming_tx));
        tokio::spawn(post_writer_task(client, post_endpoint, outgoing_rx));

        Ok(Self {
            tx: outgoing_tx,
            rx: incoming_rx,
        })
    }

    pub fn into_parts(
        self,
    ) -> (
        SseSink<ClientJsonRpcMessage>,
        SseStream<ServerJsonRpcMessage>,
    ) {
        (
            SseSink {
                tx: PollSender {
                    tx: self.tx,
                    permit: None,
                    acquiring: None,
                },
            },
            SseStream { rx: self.rx },
        )
    }
}

async fn wait_for_endpoint_event(es: &mut SseEventStream, base_url: &Url) -> Result<String> {
    let timeout = Duration::from_secs(30);
    tokio::time::timeout(timeout, async {
        while let Some(event) = es.next().await {
            match event {
                Ok(msg) if msg.event == "endpoint" => {
                    let endpoint = msg.data.trim().to_string();
                    let resolved = resolve_endpoint(&endpoint, base_url)?;
                    return Ok(resolved);
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(anyhow!(
                        "SSE connection error while waiting for endpoint event: {e}"
                    ));
                }
            }
        }
        Err(anyhow!("SSE stream closed before receiving endpoint event"))
    })
    .await
    .map_err(|_| anyhow!("Timed out waiting for endpoint event from SSE server (30s)"))?
}

fn resolve_endpoint(endpoint: &str, base_url: &Url) -> Result<String> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        Ok(endpoint.to_string())
    } else {
        let mut resolved = base_url.clone();
        let (path, query) = endpoint.split_once('?').unwrap_or((endpoint, ""));
        resolved.set_path(path);
        resolved.set_query(if query.is_empty() { None } else { Some(query) });
        Ok(resolved.to_string())
    }
}

async fn sse_reader_task(mut es: SseEventStream, tx: Sender<ServerJsonRpcMessage>) {
    while let Some(event) = es.next().await {
        match event {
            Ok(msg) if msg.event == "message" => {
                match serde_json::from_str::<ServerJsonRpcMessage>(&msg.data) {
                    Ok(rpc_msg) => {
                        if tx.send(rpc_msg).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to parse SSE message as JSON-RPC: {e}");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                error!("SSE stream error: {e}");
                break;
            }
        }
    }
}

async fn post_writer_task(
    client: Client,
    endpoint: String,
    mut rx: Receiver<ClientJsonRpcMessage>,
) {
    while let Some(msg) = rx.recv().await {
        let body = match serde_json::to_string(&msg) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialize JSON-RPC message: {e}");
                continue;
            }
        };
        if let Err(e) = client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            error!("Failed to POST message to SSE endpoint: {e}");
        }
    }
}

pub struct SseSink<T> {
    tx: PollSender<T>,
}

pub struct SseStream<T> {
    rx: Receiver<T>,
}

impl<T: Send + 'static> futures_util::Sink<T> for SseSink<T> {
    type Error = SseSinkError;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.tx.poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        self.tx.start_send(item)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl<T: Send + 'static> futures_util::Stream for SseStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[derive(Debug)]
pub enum SseSinkError {
    Closed,
}

impl Display for SseSinkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SseSinkError::Closed => write!(f, "SSE transport channel closed"),
        }
    }
}

impl Error for SseSinkError {}

type ReserveOwned<T> = Pin<Box<dyn Future<Output = Result<OwnedPermit<T>, SendError<()>>> + Send>>;

struct PollSender<T> {
    tx: Sender<T>,
    permit: Option<OwnedPermit<T>>,
    acquiring: Option<ReserveOwned<T>>,
}

impl<T: Send + 'static> PollSender<T> {
    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), SseSinkError>> {
        if self.permit.is_some() {
            return Poll::Ready(Ok(()));
        }

        let fut = self
            .acquiring
            .get_or_insert_with(|| Box::pin(self.tx.clone().reserve_owned()));

        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(permit)) => {
                self.acquiring = None;
                self.permit = Some(permit);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(_)) => {
                self.acquiring = None;
                Poll::Ready(Err(SseSinkError::Closed))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn start_send(&mut self, item: T) -> Result<(), SseSinkError> {
        let permit = self.permit.take().ok_or(SseSinkError::Closed)?;
        permit.send(item);
        Ok(())
    }
}
