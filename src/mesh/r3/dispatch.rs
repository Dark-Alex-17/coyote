use crate::mesh::destination_address;
use crate::mesh::r3::client::SizeBranch;
use crate::mesh::r3::error::RefusalCode;
use crate::mesh::r3::frame::{Envelope, PathHash, RequestId};
use crate::mesh::r3::server::{Admission, InboundRequest, Reply, RequestHandler};
use crate::mesh::r3::short;
use crate::mesh::trust::{Decision, IdentityStanding, Rule, TrustStore};

use async_trait::async_trait;
use parking_lot::RwLock;
use rmpv::Value;
use rns_transport::destination::link::LinkId;
use rns_transport::hash::AddressHash;
use rns_transport::identity::Identity;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

pub(crate) const KNOCK_PATH: &str = "/knock";
pub(crate) const STATUS_PATH: &str = "/status";
pub(crate) const MESSAGE_PATH: &str = "/message";

fn path_name(path_hash: PathHash) -> Option<&'static str> {
    [KNOCK_PATH, STATUS_PATH, MESSAGE_PATH]
        .into_iter()
        .find(|path| PathHash::of(path) == path_hash)
}

/// One request the dispatcher has let through. `identity` is proven on the link and
/// `destination_hash` is the requester's own instance, derived from the origin it named and
/// that identity. `requested_at` is the peer's timestamp verbatim (`time.time()` in RNS),
/// so it may be NaN, infinite or far from this node's clock; clamp it before using it for
/// freshness.
// `requested_at` and `branch` wait for the status and message providers.
#[allow(dead_code)]
pub(crate) struct AdmittedRequest {
    pub link_id: LinkId,
    pub identity: Identity,
    pub destination_hash: AddressHash,
    pub request_id: RequestId,
    pub path_hash: PathHash,
    pub requested_at: f64,
    pub body: Value,
    pub branch: SizeBranch,
}

/// What serves one path once the dispatcher has let a request through. Everything about
/// who may ask has been settled by then.
#[async_trait]
pub(crate) trait Handler: Send + Sync {
    async fn handle(&self, request: AdmittedRequest) -> Reply;
}

/// Where knocks go: an identity the user trusts asking from an instance the user has not
/// trusted yet, or introducing itself on `/knock` outright.
pub(crate) trait KnockSink: Send + Sync {
    fn knock(&self, knock: KnockEvent);
}

pub(crate) struct KnockEvent {
    pub identity_hash: String,
    /// The knocking instance: the requester's destination, bound to its proven identity.
    pub destination_hash: String,
    pub link_id: LinkId,
    pub path_hash: PathHash,
    /// The request body, carried only for `/knock`, where it is the knocker's introduction.
    /// It is attacker-controlled: a sink that renders it must length-cap it and strip
    /// control characters first.
    pub data: Option<Value>,
}

/// Notes each knock in the debug log and nothing more.
pub(crate) struct LoggingKnockSink;

impl KnockSink for LoggingKnockSink {
    fn knock(&self, knock: KnockEvent) {
        debug!(
            "Mesh knock from {} for destination {} on link {} via {}{}",
            short(&knock.identity_hash),
            short(&knock.destination_hash),
            knock.link_id.to_hex_string(),
            describe_path(knock.path_hash),
            if knock.data.is_some() {
                " with an introduction"
            } else {
                ""
            }
        );
    }
}

/// An answer to an allowed request that no handler could give. It travels as a msgpack
/// map, which no reading of the wire can confuse with a refusal code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchError {
    UnknownPath { path_hash: String },
    NoProvider { path: String },
}

impl DispatchError {
    pub(crate) fn to_value(&self) -> Value {
        let (kind, key, detail) = match self {
            Self::UnknownPath { path_hash } => ("unknown_path", "path_hash", path_hash),
            Self::NoProvider { path } => ("no_provider", "path", path),
        };
        Value::Map(vec![
            (Value::from("error"), Value::from(kind)),
            (Value::from(key), Value::from(detail.as_str())),
        ])
    }

    // Read by the requesting side once the status and message callers land.
    #[allow(dead_code)]
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let entries = value.as_map()?;
        let field = |name: &str| {
            entries
                .iter()
                .find(|(key, _)| key.as_str() == Some(name))
                .and_then(|(_, value)| value.as_str())
                .map(str::to_string)
        };
        match field("error")?.as_str() {
            "unknown_path" => Some(Self::UnknownPath {
                path_hash: field("path_hash")?,
            }),
            "no_provider" => Some(Self::NoProvider {
                path: field("path")?,
            }),
            _ => None,
        }
    }
}

/// A path `register` will not hand over because the dispatcher serves it itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReservedPath(pub String);

impl fmt::Display for ReservedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "The mesh path {} is served by the dispatcher itself and cannot be registered",
            self.0
        )
    }
}

impl std::error::Error for ReservedPath {}

enum Route {
    Provided(Arc<dyn Handler>),
    /// A path this node knows but nothing serves yet.
    NoProvider(&'static str),
}

/// The trust gate in front of every handler. Identity comes first and before decoding:
/// a peer the trust list does not name gets no reply and its bytes are never parsed, and
/// `handle` reads the identity's standing again so a peer untrusted or blocked after
/// `admit` let it through still gets no reply.
/// Destination comes second, and it is the requester's own: each request names the
/// instance asking, the destination judged is derived from that name and the identity
/// proven on the link, and so a peer can only claim instances that are its own. A body
/// that names no instance is refused without a knock. A known identity asking from an
/// instance it is not trusted on is refused, and knocks when that is only because nobody
/// trusted it there yet. Both verdicts are the store's; nothing here reads the trust file
/// or ranks rules itself.
pub(crate) struct Dispatcher {
    trust: Arc<TrustStore>,
    knocks: Arc<dyn KnockSink>,
    /// Read for the length of a lookup only; a handler runs with the lock released.
    routes: RwLock<HashMap<PathHash, Route>>,
}

impl Dispatcher {
    pub(crate) fn new(trust: Arc<TrustStore>, knocks: Arc<dyn KnockSink>) -> Self {
        let mut routes = HashMap::new();
        routes.insert(
            PathHash::of(KNOCK_PATH),
            Route::Provided(Arc::new(KnockHandler {
                knocks: knocks.clone(),
            })),
        );
        for path in [STATUS_PATH, MESSAGE_PATH] {
            routes.insert(PathHash::of(path), Route::NoProvider(path));
        }
        Self {
            trust,
            knocks,
            routes: RwLock::new(routes),
        }
    }

    /// Serves `path` with `handler`, returning the handler it displaces, if any. A
    /// placeholder counts as nothing displaced. `/knock` is the dispatcher's own: registering
    /// it is refused and the routes are left as they were.
    // Reached by the status and message providers once they land.
    #[allow(dead_code)]
    pub(crate) fn register(
        &self,
        path: &str,
        handler: Arc<dyn Handler>,
    ) -> Result<Option<Arc<dyn Handler>>, ReservedPath> {
        let path_hash = PathHash::of(path);
        if path_hash == PathHash::of(KNOCK_PATH) {
            return Err(ReservedPath(path.to_string()));
        }
        let displaced = match self
            .routes
            .write()
            .insert(path_hash, Route::Provided(handler))
        {
            Some(Route::Provided(previous)) => Some(previous),
            Some(Route::NoProvider(_)) | None => None,
        };
        Ok(displaced)
    }

    /// Answers a verdict the store refused under `rule`. A default-closed refusal knocks
    /// first, since nobody has trusted the instance yet. A blocked identity hears nothing,
    /// as it would have from `admit`: the block may have landed after `handle` read its
    /// standing, and the refusal taxonomy promises blocked peers silence either way.
    pub(super) fn refusal(&self, rule: Rule, knock: KnockEvent, log: &dyn Fn(&str, &str)) -> Reply {
        let id8 = short(&knock.identity_hash).to_string();
        match rule {
            Rule::IdentityBlocked => {
                log(&id8, "dropped: blocked identity");
                Reply::Silent
            }
            Rule::DefaultClosed => {
                self.knocks.knock(knock);
                log(&id8, &format!("refused: {rule:?} (knocked)"));
                self.refuse()
            }
            _ => {
                log(&id8, &format!("refused: {rule:?}"));
                self.refuse()
            }
        }
    }

    /// The one place a refusal is built, so every refusal is the same bytes on the wire
    /// whichever rule produced it.
    fn refuse(&self) -> Reply {
        Reply::Code(RefusalCode::NoAccess)
    }
}

#[async_trait]
impl RequestHandler for Dispatcher {
    fn admit(&self, link_id: LinkId, identity: Option<&Identity>) -> Admission {
        let (id8, outcome) = match identity {
            None => ("anonymous".to_string(), "dropped: unauthenticated"),
            Some(identity) => {
                let identity_hex = identity.address_hash.to_hex_string();
                let outcome = match self.trust.identity_standing(&identity_hex) {
                    IdentityStanding::Trusted { .. } => return Admission::Admit,
                    IdentityStanding::Unknown => "dropped: unknown identity",
                    IdentityStanding::Blocked => "dropped: blocked identity",
                };
                (short(&identity_hex).to_string(), outcome)
            }
        };
        debug!(
            "Mesh request (not yet decoded) from {id8} on link {}: {outcome}",
            link_id.to_hex_string()
        );
        Admission::Drop
    }

    async fn handle(&self, request: InboundRequest) -> Reply {
        let path = describe_path(request.path_hash);
        let (request_id, link_id) = (request.request_id, request.link_id);
        let log = |id8: &str, outcome: &str| {
            debug!(
                "Mesh request {} for {path} from {id8} on link {}: {outcome}",
                request_id.to_hex_string(),
                link_id.to_hex_string()
            );
        };
        let Some(identity) = request.identity else {
            log("anonymous", "dropped: unauthenticated");
            return Reply::Silent;
        };
        let identity_hex = identity.address_hash.to_hex_string();
        let dropped = match self.trust.identity_standing(&identity_hex) {
            IdentityStanding::Trusted { .. } => None,
            IdentityStanding::Unknown => Some("dropped: unknown identity"),
            IdentityStanding::Blocked => Some("dropped: blocked identity"),
        };
        if let Some(outcome) = dropped {
            log(short(&identity_hex), outcome);
            return Reply::Silent;
        }
        let Some(envelope) = Envelope::from_value(request.data) else {
            log(short(&identity_hex), "refused: unverifiable origin");
            return self.refuse();
        };
        let destination_hash = destination_address(&envelope.origin.0, &identity.address_hash);
        let destination_hex = destination_hash.to_hex_string();
        let verdict = self.trust.authorize(&identity_hex, &destination_hex);
        let rule = verdict.rule;
        match verdict.decision {
            Decision::Refuse => {
                let data = (request.path_hash == PathHash::of(KNOCK_PATH)).then_some(envelope.body);
                self.refusal(
                    rule,
                    KnockEvent {
                        identity_hash: identity_hex,
                        destination_hash: destination_hex,
                        link_id,
                        path_hash: request.path_hash,
                        data,
                    },
                    &log,
                )
            }
            Decision::Allow => {
                let route = self
                    .routes
                    .read()
                    .get(&request.path_hash)
                    .map(|route| match route {
                        Route::Provided(handler) => Route::Provided(handler.clone()),
                        Route::NoProvider(path) => Route::NoProvider(path),
                    });
                match route {
                    None => {
                        log(short(&identity_hex), &format!("unknown path: {rule:?}"));
                        Reply::Value(
                            DispatchError::UnknownPath {
                                path_hash: request.path_hash.to_hex_string(),
                            }
                            .to_value(),
                        )
                    }
                    Some(Route::NoProvider(path)) => {
                        log(short(&identity_hex), &format!("no provider: {rule:?}"));
                        Reply::Value(
                            DispatchError::NoProvider {
                                path: path.to_string(),
                            }
                            .to_value(),
                        )
                    }
                    Some(Route::Provided(handler)) => {
                        log(short(&identity_hex), &format!("served: {rule:?}"));
                        handler
                            .handle(AdmittedRequest {
                                link_id,
                                identity,
                                destination_hash,
                                request_id,
                                path_hash: request.path_hash,
                                requested_at: request.requested_at,
                                body: envelope.body,
                                branch: request.branch,
                            })
                            .await
                    }
                }
            }
        }
    }
}

fn describe_path(path_hash: PathHash) -> String {
    match path_name(path_hash) {
        Some(name) => name.to_string(),
        None => format!("hash {}", path_hash.to_hex_string()),
    }
}

/// `/knock` itself: the request body is the introduction, and reaching here means the
/// knocking instance is already trusted, so the knock is noted and acknowledged with nil.
struct KnockHandler {
    knocks: Arc<dyn KnockSink>,
}

#[async_trait]
impl Handler for KnockHandler {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        self.knocks.knock(KnockEvent {
            identity_hash: request.identity.address_hash.to_hex_string(),
            destination_hash: request.destination_hash.to_hex_string(),
            link_id: request.link_id,
            path_hash: request.path_hash,
            data: Some(request.body),
        });
        Reply::Value(Value::Nil)
    }
}
