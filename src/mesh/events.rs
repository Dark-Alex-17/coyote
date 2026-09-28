//! The `mesh.*` hook events and the seam they leave the mesh through. The node, slot,
//! trust store and knock gate build a `MeshEvent` and hand it to a shared `MeshHooks`
//! handle; whoever owns the config installs the `MeshHookSink` that resolves and runs the
//! hooks. Every env key is spelled here and nowhere else, and `MESH_HOOK_ENVS` is the
//! allow-list `fire` applies before a value leaves: hashes are full lower-hex, peer text
//! goes through `display_text`, and message, bulletin and brief content never appear.

use crate::hooks::HookEvent;
use crate::mesh::card::DISPLAY_NAME_MAX_CHARS;
use crate::mesh::knock::KnockVia;
use crate::mesh::knocks::KNOCK_INTRO_MAX_CHARS;
use crate::mesh::message::{BulletinTally, PEER_TITLE_MAX_CHARS, PeerKind, PeerVia};
use crate::mesh::peers::PeerChange;
use crate::mesh::protocol::Compatibility;
use crate::mesh::trust::{RevokeReason, Tier, TrustGranted, TrustObserver, TrustRevoked};
use crate::mesh::{canonical_hash, display_text};

use arc_swap::ArcSwapOption;
use std::sync::Arc;

/// Runs the hooks for one event. Called from request handlers, the announce task and
/// under the trust store's lock, so it must not block; mesh events carry no payload
/// today, and `payload` is always `None`. Mesh events are node-level: only the global
/// `hooks:` map applies and no agent whitelist is consulted, even when an agent's
/// `mesh__*` tool call triggered the fire.
pub(crate) trait MeshHookSink: Send + Sync {
    fn fire(&self, event: HookEvent, extras: Vec<(&'static str, String)>, payload: Option<String>);
}

/// A cheap shared handle on the installed sink. Every mesh component holds a clone of
/// the same handle, so a sink set before or after the node starts is seen by all of them.
#[derive(Clone, Default)]
pub(crate) struct MeshHooks(Arc<ArcSwapOption<Arc<dyn MeshHookSink>>>);

impl MeshHooks {
    pub(crate) fn set(&self, sink: Arc<dyn MeshHookSink>) {
        self.0.store(Some(Arc::new(sink)));
    }

    pub(crate) fn clear(&self) {
        self.0.store(None);
    }

    pub(crate) fn current(&self) -> Option<Arc<dyn MeshHookSink>> {
        self.0.load_full().map(|sink| Arc::clone(&*sink))
    }

    /// Whether `other` is a clone of this handle rather than a separately built one.
    pub(crate) fn same_handle(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// With no sink installed the event is dropped.
    pub(crate) fn fire(&self, event: MeshEvent) {
        let Some(sink) = self.0.load_full() else {
            return;
        };
        let hook_event = event.hook_event();
        let allowed = allowed_envs(hook_event);
        let mut envs = event.envs();
        envs.retain(|(key, _)| allowed.contains(key));
        sink.fire(hook_event, envs, None);
    }
}

/// This node as `mesh.started` and `mesh.stopped` describe it.
pub(crate) struct NodeFacts {
    pub instance_id: String,
    pub destination: String,
    pub identity: String,
    pub interfaces: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Routed {
    Envoy,
    Inbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BriefUpdateSource {
    User,
    Digest,
}

pub(crate) enum MeshEvent {
    Started(NodeFacts),
    Stopped(NodeFacts),
    PeerDiscovered {
        destination: String,
        identity: String,
        name: Option<String>,
        hops: u8,
        protocol_version: u16,
        compatibility: Compatibility,
        change: PeerChange,
    },
    KnockReceived {
        identity: String,
        destination: String,
        name: Option<String>,
        intro: Option<String>,
        via: KnockVia,
    },
    TrustGranted {
        tier: Tier,
        identity: String,
        destination: Option<String>,
    },
    TrustRevoked {
        tier: Tier,
        identity: Option<String>,
        destination: Option<String>,
        reason: RevokeReason,
    },
    MessageReceived {
        kind: PeerKind,
        id: String,
        in_reply_to: Option<String>,
        identity: String,
        destination: String,
        title: Option<String>,
        via: PeerVia,
        routed: Routed,
    },
    MessageSent {
        kind: PeerKind,
        id: String,
        destination: Option<String>,
        via: PeerVia,
    },
    MessageFailed {
        kind: PeerKind,
        id: String,
        destination: Option<String>,
        class: &'static str,
        error: String,
    },
    BulletinReceived {
        id: String,
        identity: String,
        destination: String,
        title: Option<String>,
        via: PeerVia,
    },
    BulletinSent {
        id: String,
        tally: BulletinTally,
    },
    BriefUpdated {
        source: BriefUpdateSource,
        chars: usize,
    },
}

const INSTANCE_ID: &str = "COYOTE_MESH_INSTANCE_ID";
const DESTINATION: &str = "COYOTE_MESH_DESTINATION";
const IDENTITY: &str = "COYOTE_MESH_IDENTITY";
const INTERFACES: &str = "COYOTE_MESH_INTERFACES";
const PEER_DESTINATION: &str = "COYOTE_MESH_PEER_DESTINATION";
const PEER_IDENTITY: &str = "COYOTE_MESH_PEER_IDENTITY";
const PEER_NAME: &str = "COYOTE_MESH_PEER_NAME";
const HOPS: &str = "COYOTE_MESH_HOPS";
const PEER_PROTOCOL_VERSION: &str = "COYOTE_MESH_PEER_PROTOCOL_VERSION";
const PEER_COMPATIBLE: &str = "COYOTE_MESH_PEER_COMPATIBLE";
const FIRST_SEEN: &str = "COYOTE_MESH_FIRST_SEEN";
const KNOCK_INTRO: &str = "COYOTE_MESH_KNOCK_INTRO";
const VIA: &str = "COYOTE_MESH_VIA";
const TRUST_TIER: &str = "COYOTE_MESH_TRUST_TIER";
const TRUST_REASON: &str = "COYOTE_MESH_TRUST_REASON";
const MESSAGE_KIND: &str = "COYOTE_MESH_MESSAGE_KIND";
const MESSAGE_ID: &str = "COYOTE_MESH_MESSAGE_ID";
const IN_REPLY_TO: &str = "COYOTE_MESH_IN_REPLY_TO";
const MESSAGE_TITLE: &str = "COYOTE_MESH_MESSAGE_TITLE";
const ROUTED: &str = "COYOTE_MESH_ROUTED";
const ERROR_CLASS: &str = "COYOTE_MESH_ERROR_CLASS";
const ERROR: &str = "COYOTE_MESH_ERROR";
const RECIPIENTS: &str = "COYOTE_MESH_RECIPIENTS";
const DELIVERED: &str = "COYOTE_MESH_DELIVERED";
const STORED: &str = "COYOTE_MESH_STORED";
const UNREACHABLE: &str = "COYOTE_MESH_UNREACHABLE";
const REFUSED: &str = "COYOTE_MESH_REFUSED";
const BRIEF_SOURCE: &str = "COYOTE_MESH_BRIEF_SOURCE";
const BRIEF_CHARS: &str = "COYOTE_MESH_BRIEF_CHARS";

const ERROR_MAX_CHARS: usize = 512;

const NODE_ENVS: &[&str] = &[INSTANCE_ID, DESTINATION, IDENTITY, INTERFACES];

/// Every env each event may set. `fire` drops anything else, and the tests hold
/// `MeshEvent::envs` to exactly these keys.
pub(crate) const MESH_HOOK_ENVS: &[(HookEvent, &[&str])] = &[
    (HookEvent::MeshStarted, NODE_ENVS),
    (HookEvent::MeshStopped, NODE_ENVS),
    (
        HookEvent::MeshPeerDiscovered,
        &[
            PEER_DESTINATION,
            PEER_IDENTITY,
            PEER_NAME,
            HOPS,
            PEER_PROTOCOL_VERSION,
            PEER_COMPATIBLE,
            FIRST_SEEN,
        ],
    ),
    (
        HookEvent::MeshKnockReceived,
        &[PEER_IDENTITY, PEER_DESTINATION, PEER_NAME, KNOCK_INTRO, VIA],
    ),
    (
        HookEvent::MeshTrustGranted,
        &[TRUST_TIER, PEER_IDENTITY, PEER_DESTINATION],
    ),
    (
        HookEvent::MeshTrustRevoked,
        &[TRUST_TIER, PEER_IDENTITY, PEER_DESTINATION, TRUST_REASON],
    ),
    (
        HookEvent::MeshMessageReceived,
        &[
            MESSAGE_KIND,
            MESSAGE_ID,
            IN_REPLY_TO,
            PEER_IDENTITY,
            PEER_DESTINATION,
            MESSAGE_TITLE,
            VIA,
            ROUTED,
        ],
    ),
    (
        HookEvent::MeshMessageSent,
        &[MESSAGE_KIND, MESSAGE_ID, PEER_DESTINATION, VIA],
    ),
    (
        HookEvent::MeshMessageFailed,
        &[
            MESSAGE_KIND,
            MESSAGE_ID,
            PEER_DESTINATION,
            ERROR_CLASS,
            ERROR,
        ],
    ),
    (
        HookEvent::MeshBulletinReceived,
        &[
            MESSAGE_KIND,
            MESSAGE_ID,
            PEER_IDENTITY,
            PEER_DESTINATION,
            MESSAGE_TITLE,
            VIA,
        ],
    ),
    (
        HookEvent::MeshBulletinSent,
        &[
            MESSAGE_ID,
            RECIPIENTS,
            DELIVERED,
            STORED,
            UNREACHABLE,
            REFUSED,
        ],
    ),
    (HookEvent::MeshBriefUpdated, &[BRIEF_SOURCE, BRIEF_CHARS]),
];

fn allowed_envs(event: HookEvent) -> &'static [&'static str] {
    MESH_HOOK_ENVS
        .iter()
        .find(|(known, _)| *known == event)
        .map_or(&[], |(_, keys)| keys)
}

fn peer_via(via: PeerVia) -> &'static str {
    match via {
        PeerVia::Direct => "direct",
        PeerVia::StoreAndForward => "store-and-forward",
    }
}

fn knock_via(via: KnockVia) -> &'static str {
    match via {
        KnockVia::Direct => "direct",
        KnockVia::StoreAndForward => "store-and-forward",
    }
}

fn push_text(
    envs: &mut Vec<(&'static str, String)>,
    key: &'static str,
    text: Option<&str>,
    max_chars: usize,
) {
    if let Some(text) = text.and_then(|text| display_text(text, max_chars)) {
        envs.push((key, text));
    }
}

fn push_opt(envs: &mut Vec<(&'static str, String)>, key: &'static str, value: Option<String>) {
    if let Some(value) = value {
        envs.push((key, value));
    }
}

impl MeshEvent {
    pub(crate) fn hook_event(&self) -> HookEvent {
        match self {
            Self::Started(_) => HookEvent::MeshStarted,
            Self::Stopped(_) => HookEvent::MeshStopped,
            Self::PeerDiscovered { .. } => HookEvent::MeshPeerDiscovered,
            Self::KnockReceived { .. } => HookEvent::MeshKnockReceived,
            Self::TrustGranted { .. } => HookEvent::MeshTrustGranted,
            Self::TrustRevoked { .. } => HookEvent::MeshTrustRevoked,
            Self::MessageReceived { .. } => HookEvent::MeshMessageReceived,
            Self::MessageSent { .. } => HookEvent::MeshMessageSent,
            Self::MessageFailed { .. } => HookEvent::MeshMessageFailed,
            Self::BulletinReceived { .. } => HookEvent::MeshBulletinReceived,
            Self::BulletinSent { .. } => HookEvent::MeshBulletinSent,
            Self::BriefUpdated { .. } => HookEvent::MeshBriefUpdated,
        }
    }

    /// The event's envs; an `Option` that is `None` sets nothing.
    pub(crate) fn envs(&self) -> Vec<(&'static str, String)> {
        let mut envs = Vec::new();
        match self {
            Self::Started(node) | Self::Stopped(node) => {
                envs.push((INSTANCE_ID, node.instance_id.clone()));
                envs.push((DESTINATION, node.destination.clone()));
                envs.push((IDENTITY, node.identity.clone()));
                envs.push((INTERFACES, node.interfaces.join(",")));
            }
            Self::PeerDiscovered {
                destination,
                identity,
                name,
                hops,
                protocol_version,
                compatibility,
                change,
            } => {
                envs.push((PEER_DESTINATION, destination.clone()));
                envs.push((PEER_IDENTITY, identity.clone()));
                push_text(
                    &mut envs,
                    PEER_NAME,
                    name.as_deref(),
                    DISPLAY_NAME_MAX_CHARS,
                );
                envs.push((HOPS, hops.to_string()));
                envs.push((PEER_PROTOCOL_VERSION, protocol_version.to_string()));
                envs.push((
                    PEER_COMPATIBLE,
                    (*compatibility == Compatibility::Compatible).to_string(),
                ));
                envs.push((FIRST_SEEN, (*change == PeerChange::Added).to_string()));
            }
            Self::KnockReceived {
                identity,
                destination,
                name,
                intro,
                via,
            } => {
                envs.push((PEER_IDENTITY, identity.clone()));
                envs.push((PEER_DESTINATION, destination.clone()));
                push_text(
                    &mut envs,
                    PEER_NAME,
                    name.as_deref(),
                    DISPLAY_NAME_MAX_CHARS,
                );
                push_text(
                    &mut envs,
                    KNOCK_INTRO,
                    intro.as_deref(),
                    KNOCK_INTRO_MAX_CHARS,
                );
                envs.push((VIA, knock_via(*via).to_string()));
            }
            Self::TrustGranted {
                tier,
                identity,
                destination,
            } => {
                envs.push((TRUST_TIER, tier.as_str().to_string()));
                envs.push((PEER_IDENTITY, identity.clone()));
                push_opt(&mut envs, PEER_DESTINATION, destination.clone());
            }
            Self::TrustRevoked {
                tier,
                identity,
                destination,
                reason,
            } => {
                envs.push((TRUST_TIER, tier.as_str().to_string()));
                push_opt(&mut envs, PEER_IDENTITY, identity.clone());
                push_opt(&mut envs, PEER_DESTINATION, destination.clone());
                envs.push((TRUST_REASON, reason.as_str().to_string()));
            }
            Self::MessageReceived {
                kind,
                id,
                in_reply_to,
                identity,
                destination,
                title,
                via,
                routed,
            } => {
                envs.push((MESSAGE_KIND, kind.to_string()));
                envs.push((MESSAGE_ID, id.clone()));
                push_opt(&mut envs, IN_REPLY_TO, in_reply_to.clone());
                envs.push((PEER_IDENTITY, identity.clone()));
                envs.push((PEER_DESTINATION, destination.clone()));
                push_text(
                    &mut envs,
                    MESSAGE_TITLE,
                    title.as_deref(),
                    PEER_TITLE_MAX_CHARS,
                );
                envs.push((VIA, peer_via(*via).to_string()));
                let routed = match routed {
                    Routed::Envoy => "envoy",
                    Routed::Inbox => "inbox",
                };
                envs.push((ROUTED, routed.to_string()));
            }
            Self::MessageSent {
                kind,
                id,
                destination,
                via,
            } => {
                envs.push((MESSAGE_KIND, kind.to_string()));
                envs.push((MESSAGE_ID, id.clone()));
                push_opt(&mut envs, PEER_DESTINATION, destination.clone());
                envs.push((VIA, peer_via(*via).to_string()));
            }
            Self::MessageFailed {
                kind,
                id,
                destination,
                class,
                error,
            } => {
                envs.push((MESSAGE_KIND, kind.to_string()));
                envs.push((MESSAGE_ID, id.clone()));
                push_opt(&mut envs, PEER_DESTINATION, destination.clone());
                envs.push((ERROR_CLASS, class.to_string()));
                push_text(&mut envs, ERROR, Some(error.as_str()), ERROR_MAX_CHARS);
            }
            Self::BulletinReceived {
                id,
                identity,
                destination,
                title,
                via,
            } => {
                envs.push((MESSAGE_KIND, PeerKind::Bulletin.to_string()));
                envs.push((MESSAGE_ID, id.clone()));
                envs.push((PEER_IDENTITY, identity.clone()));
                envs.push((PEER_DESTINATION, destination.clone()));
                push_text(
                    &mut envs,
                    MESSAGE_TITLE,
                    title.as_deref(),
                    PEER_TITLE_MAX_CHARS,
                );
                envs.push((VIA, peer_via(*via).to_string()));
            }
            Self::BulletinSent { id, tally } => {
                envs.push((MESSAGE_ID, id.clone()));
                envs.push((RECIPIENTS, tally.recipients.to_string()));
                envs.push((DELIVERED, tally.delivered.to_string()));
                envs.push((STORED, tally.stored.to_string()));
                envs.push((UNREACHABLE, tally.unreachable.to_string()));
                envs.push((REFUSED, tally.refused.to_string()));
            }
            Self::BriefUpdated { source, chars } => {
                let source = match source {
                    BriefUpdateSource::User => "user",
                    BriefUpdateSource::Digest => "digest",
                };
                envs.push((BRIEF_SOURCE, source.to_string()));
                envs.push((BRIEF_CHARS, chars.to_string()));
            }
        }
        envs
    }
}

/// The trust store's observer: each grant and revocation becomes a `mesh.trust.*` event.
/// A destination hash that is not canonical is dropped rather than forwarded.
pub(crate) struct TrustHookObserver(pub(crate) MeshHooks);

impl TrustObserver for TrustHookObserver {
    fn granted(&self, event: TrustGranted) {
        self.0.fire(MeshEvent::TrustGranted {
            tier: event.tier,
            identity: event.identity_hash,
            destination: event.destination_hash.as_deref().and_then(canonical_hash),
        });
    }

    fn revoked(&self, event: TrustRevoked) {
        self.0.fire(MeshEvent::TrustRevoked {
            tier: event.tier,
            identity: event.identity_hash,
            destination: event.destination_hash.as_deref().and_then(canonical_hash),
            reason: event.reason,
        });
    }
}

#[cfg(test)]
pub(crate) type RecordedFire = (HookEvent, Vec<(&'static str, String)>);

/// Keeps every fire it is handed, for tests at the fire sites.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingHookSink(parking_lot::Mutex<Vec<RecordedFire>>);

#[cfg(test)]
impl RecordingHookSink {
    pub(crate) fn attach(hooks: &MeshHooks) -> Arc<Self> {
        let sink = Arc::new(Self::default());
        hooks.set(Arc::clone(&sink) as Arc<dyn MeshHookSink>);
        sink
    }

    pub(crate) fn drain(&self) -> Vec<RecordedFire> {
        std::mem::take(&mut *self.0.lock())
    }

    pub(crate) fn snapshot(&self) -> Vec<RecordedFire> {
        self.0.lock().clone()
    }
}

#[cfg(test)]
impl MeshHookSink for RecordingHookSink {
    fn fire(&self, event: HookEvent, extras: Vec<(&'static str, String)>, payload: Option<String>) {
        assert!(payload.is_none(), "mesh events carry no payload");
        self.0.lock().push((event, extras));
    }
}

/// The value of `key` in one recorded fire.
#[cfg(test)]
pub(crate) fn env_value<'a>(envs: &'a [(&'static str, String)], key: &str) -> Option<&'a str> {
    envs.iter()
        .find(|(known, _)| *known == key)
        .map(|(_, value)| value.as_str())
}

/// Drains the sink, asserting it holds exactly one fire of `event`, and returns its envs.
#[cfg(test)]
pub(crate) fn one_fire(sink: &RecordingHookSink, event: HookEvent) -> Vec<(&'static str, String)> {
    let mut fired = sink.drain();
    assert_eq!(fired.len(), 1, "{fired:?}");
    let (fired_event, envs) = fired.remove(0);
    assert_eq!(fired_event, event);
    envs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::destination_address;
    use crate::mesh::identity::fingerprint;
    use crate::mesh::r3::NAME_HASH_LEN;
    use lxmf_core::identity::PrivateIdentity;
    use rns_transport::identity_bridge::to_transport_private_identity;
    use std::collections::BTreeSet;

    /// One event per variant with every `Option` set, all hashes derived from `identity`.
    fn fixtures(identity: &PrivateIdentity) -> Vec<MeshEvent> {
        let fingerprint = fingerprint(identity);
        let destination = destination_address(
            &[7u8; NAME_HASH_LEN],
            to_transport_private_identity(identity).address_hash(),
        )
        .to_hex_string();
        let node = || NodeFacts {
            instance_id: "inst-1".to_string(),
            destination: destination.clone(),
            identity: fingerprint.clone(),
            interfaces: vec!["lan", "private"],
        };
        vec![
            MeshEvent::Started(node()),
            MeshEvent::Stopped(node()),
            MeshEvent::PeerDiscovered {
                destination: destination.clone(),
                identity: fingerprint.clone(),
                name: Some("Bea".to_string()),
                hops: 2,
                protocol_version: 1,
                compatibility: Compatibility::Compatible,
                change: PeerChange::Added,
            },
            MeshEvent::KnockReceived {
                identity: fingerprint.clone(),
                destination: destination.clone(),
                name: Some("Bea".to_string()),
                intro: Some("hello".to_string()),
                via: KnockVia::Direct,
            },
            MeshEvent::TrustGranted {
                tier: Tier::Destination,
                identity: fingerprint.clone(),
                destination: Some(destination.clone()),
            },
            MeshEvent::TrustRevoked {
                tier: Tier::Destination,
                identity: Some(fingerprint.clone()),
                destination: Some(destination.clone()),
                reason: RevokeReason::Prune,
            },
            MeshEvent::MessageReceived {
                kind: PeerKind::Reply,
                id: "m-1".to_string(),
                in_reply_to: Some("q-1".to_string()),
                identity: fingerprint.clone(),
                destination: destination.clone(),
                title: Some("Re: plan".to_string()),
                via: PeerVia::StoreAndForward,
                routed: Routed::Inbox,
            },
            MeshEvent::MessageSent {
                kind: PeerKind::Ask,
                id: "m-2".to_string(),
                destination: Some(destination.clone()),
                via: PeerVia::Direct,
            },
            MeshEvent::MessageFailed {
                kind: PeerKind::Message,
                id: "m-3".to_string(),
                destination: Some(destination.clone()),
                class: "not_trusted",
                error: "not trusted".to_string(),
            },
            MeshEvent::BulletinReceived {
                id: "b-1".to_string(),
                identity: fingerprint.clone(),
                destination: destination.clone(),
                title: Some("news".to_string()),
                via: PeerVia::Direct,
            },
            MeshEvent::BulletinSent {
                id: "b-2".to_string(),
                tally: BulletinTally {
                    recipients: 4,
                    delivered: 1,
                    stored: 1,
                    unreachable: 1,
                    refused: 1,
                },
            },
            MeshEvent::BriefUpdated {
                source: BriefUpdateSource::Digest,
                chars: 42,
            },
        ]
    }

    fn keys(envs: &[(&'static str, String)]) -> BTreeSet<&'static str> {
        envs.iter().map(|(key, _)| *key).collect()
    }

    #[test]
    fn every_event_maps_to_its_own_mesh_hook_and_exactly_its_allowed_envs() {
        let identity = PrivateIdentity::from_private_key_bytes(&[9u8; 64]).unwrap();
        let fixtures = fixtures(&identity);
        assert_eq!(fixtures.len(), MESH_HOOK_ENVS.len());
        assert_eq!(fixtures.len(), 12);

        let mut seen = BTreeSet::new();
        for event in &fixtures {
            let hook_event = event.hook_event();
            assert!(
                hook_event.as_str().starts_with("mesh."),
                "{}",
                hook_event.as_str()
            );
            assert!(
                seen.insert(hook_event.as_str()),
                "{} twice",
                hook_event.as_str()
            );
            let expected: BTreeSet<&str> = allowed_envs(hook_event).iter().copied().collect();
            assert!(
                !expected.is_empty(),
                "{} has no allow-list row",
                hook_event.as_str()
            );
            assert_eq!(
                keys(&event.envs()),
                expected,
                "{} envs drifted from MESH_HOOK_ENVS",
                hook_event.as_str()
            );
        }
        for (event, _) in MESH_HOOK_ENVS {
            assert!(
                crate::hooks::HookEvent::ALL.contains(event),
                "{} is not in HookEvent::ALL",
                event.as_str()
            );
        }
    }

    #[test]
    fn no_env_value_carries_the_private_key() {
        let identity = PrivateIdentity::from_private_key_bytes(&[9u8; 64]).unwrap();
        let private_hex = identity.to_hex_string();
        let public = fingerprint(&identity);
        assert_eq!(private_hex.len(), 128);
        let windows: Vec<&str> = (0..=private_hex.len() - 8)
            .map(|at| &private_hex[at..at + 8])
            .filter(|window| !public.contains(window))
            .collect();
        assert!(!windows.is_empty());

        for event in fixtures(&identity) {
            for (key, value) in event.envs() {
                assert!(!value.contains(&private_hex), "{key} carries the key");
                for window in &windows {
                    assert!(
                        !value.contains(window),
                        "{key}={value} carries {window} of the private key"
                    );
                }
            }
        }
    }

    #[test]
    fn optional_envs_are_omitted_and_peer_text_is_cleaned() {
        let envs = MeshEvent::KnockReceived {
            identity: "id".to_string(),
            destination: "dest".to_string(),
            name: None,
            intro: Some(" \u{1b}[31mhi\u{202E} there\r\n".to_string()),
            via: KnockVia::StoreAndForward,
        }
        .envs();
        assert_eq!(env_value(&envs, PEER_NAME), None);
        assert_eq!(env_value(&envs, KNOCK_INTRO), Some("hi there"));
        assert_eq!(env_value(&envs, VIA), Some("store-and-forward"));

        let envs = MeshEvent::TrustRevoked {
            tier: Tier::Identity,
            identity: Some("id".to_string()),
            destination: None,
            reason: RevokeReason::Block,
        }
        .envs();
        assert_eq!(env_value(&envs, TRUST_TIER), Some("identity"));
        assert_eq!(env_value(&envs, PEER_DESTINATION), None);
        assert_eq!(env_value(&envs, TRUST_REASON), Some("block"));

        let envs = MeshEvent::PeerDiscovered {
            destination: "dest".to_string(),
            identity: "id".to_string(),
            name: Some("   ".to_string()),
            hops: 1,
            protocol_version: 7,
            compatibility: Compatibility::Incompatible { found: 7 },
            change: PeerChange::Refreshed,
        }
        .envs();
        assert_eq!(env_value(&envs, PEER_NAME), None);
        assert_eq!(env_value(&envs, PEER_COMPATIBLE), Some("false"));
        assert_eq!(env_value(&envs, FIRST_SEEN), Some("false"));

        let envs = MeshEvent::MessageFailed {
            kind: PeerKind::Message,
            id: "m-9".to_string(),
            destination: None,
            class: "not_trusted",
            error: "boom \u{1b}]0;x\u{7} done".to_string(),
        }
        .envs();
        assert_eq!(env_value(&envs, PEER_DESTINATION), None);
        assert_eq!(env_value(&envs, ERROR_CLASS), Some("not_trusted"));
        let error = env_value(&envs, ERROR).unwrap();
        assert!(!error.contains('\u{1b}'), "{error:?}");
        assert!(!error.contains('\u{7}'), "{error:?}");
        assert!(error.starts_with("boom"), "{error:?}");
        assert!(error.ends_with("done"), "{error:?}");

        let envs = MeshEvent::MessageFailed {
            kind: PeerKind::Message,
            id: "m-10".to_string(),
            destination: None,
            class: "direct",
            error: " \u{1b}[2J ".to_string(),
        }
        .envs();
        assert_eq!(env_value(&envs, ERROR), None);
    }

    #[test]
    fn the_handle_drops_events_without_a_sink_and_shares_one_across_clones() {
        let hooks = MeshHooks::default();
        hooks.fire(MeshEvent::BriefUpdated {
            source: BriefUpdateSource::User,
            chars: 1,
        });

        let twin = hooks.clone();
        let sink = RecordingHookSink::attach(&hooks);
        twin.fire(MeshEvent::BriefUpdated {
            source: BriefUpdateSource::User,
            chars: 3,
        });
        let fired = sink.drain();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, HookEvent::MeshBriefUpdated);
        assert_eq!(env_value(&fired[0].1, BRIEF_SOURCE), Some("user"));
        assert_eq!(env_value(&fired[0].1, BRIEF_CHARS), Some("3"));

        hooks.clear();
        twin.fire(MeshEvent::BriefUpdated {
            source: BriefUpdateSource::Digest,
            chars: 5,
        });
        assert!(sink.snapshot().is_empty());
    }

    #[test]
    fn the_trust_observer_maps_both_directions() {
        let hooks = MeshHooks::default();
        let sink = RecordingHookSink::attach(&hooks);
        let observer = TrustHookObserver(hooks);
        observer.granted(TrustGranted {
            tier: Tier::Identity,
            identity_hash: "id".to_string(),
            destination_hash: None,
        });
        let destination = "AB".repeat(16);
        observer.revoked(TrustRevoked {
            tier: Tier::Destination,
            identity_hash: Some("id".to_string()),
            destination_hash: Some(destination),
            reason: RevokeReason::Untrust,
        });
        observer.revoked(TrustRevoked {
            tier: Tier::Destination,
            identity_hash: Some("id".to_string()),
            destination_hash: Some("dest".to_string()),
            reason: RevokeReason::Untrust,
        });
        let fired = sink.drain();
        assert_eq!(fired.len(), 3, "{fired:?}");
        assert_eq!(fired[0].0, HookEvent::MeshTrustGranted);
        assert_eq!(env_value(&fired[0].1, TRUST_TIER), Some("identity"));
        assert_eq!(env_value(&fired[0].1, PEER_DESTINATION), None);
        assert_eq!(fired[1].0, HookEvent::MeshTrustRevoked);
        assert_eq!(
            env_value(&fired[1].1, PEER_DESTINATION),
            Some("ab".repeat(16).as_str())
        );
        assert_eq!(env_value(&fired[1].1, TRUST_REASON), Some("untrust"));
        assert_eq!(
            env_value(&fired[2].1, PEER_DESTINATION),
            None,
            "a hash that is not canonical is dropped"
        );
    }
}
