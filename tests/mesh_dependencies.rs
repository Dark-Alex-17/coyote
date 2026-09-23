//! Smoke coverage for the Reticulum/LXMF dependencies.
//!
//! These crates enter the tree ahead of the code that uses them, so nothing else
//! would notice if the pinned revision stopped resolving, stopped exposing the
//! identity and wire surface, or lost the `storage` feature that brings bundled
//! SQLite in. The two captured vectors go further and pin the protocol itself:
//! Reticulum addresses peers by a truncated hash of their public keys, and LXMF
//! peers agree on an exact frame layout, so a change to either is an interop
//! break that should fail here rather than against a live Python node.

use lxmf_core::identity::{Identity, PrivateIdentity, lxmf_sign, lxmf_verify};
use lxmf_core::{Message, WireMessage};
use rns_transport::storage::messages::MessagesStore;
use sha2::{Digest, Sha256};

/// Fixed X25519 secret and Ed25519 signing key, 32 bytes each. Chosen arbitrarily;
/// what matters is that they never change, so the vectors below stay comparable.
const SENDER_KEY: [u8; 64] = [0x11; 64];
const RECIPIENT_KEY: [u8; 64] = [0x22; 64];

/// Address hash `SENDER_KEY` derives to, captured from LXMF-rs at the pinned
/// revision. Pins the public-key to address-hash derivation, which is protocol.
const SENDER_ADDRESS_HASH: &str = "ef330a1940c70349459fc4401d273cb9";

/// SHA-256 of the packed frame `lxmf_wire_frames_are_byte_stable` builds. The
/// frame is deterministic: fixed keys, a fixed timestamp, no fields, and Ed25519
/// signing is deterministic, so any change to the encoding shows up here.
const WIRE_FRAME_DIGEST: &str = "9a7be88510c749b17fb240822805e30077412d1ec9a6fd25908ad29329460eed";

/// Timestamp baked into the frame vector. Held constant so the encoding is.
const FIXED_TIMESTAMP: f64 = 1_700_000_000.0;

fn identity_from(key: [u8; 64]) -> PrivateIdentity {
    PrivateIdentity::from_private_key_bytes(&key).expect("64 key bytes are a valid identity")
}

fn address_bytes(identity: &PrivateIdentity) -> [u8; 16] {
    identity
        .address_hash()
        .as_slice()
        .try_into()
        .expect("a Reticulum address hash is 16 bytes")
}

fn signed_frame(sender: &PrivateIdentity, recipient: &PrivateIdentity) -> Vec<u8> {
    let mut message = Message::new();
    message.source_hash = Some(address_bytes(sender));
    message.destination_hash = Some(address_bytes(recipient));
    message.timestamp = Some(FIXED_TIMESTAMP);
    message.set_title_from_string("smoke");
    message.set_content_from_string("hello mesh");

    message
        .to_wire(Some(sender))
        .expect("a signed message encodes")
}

#[test]
fn public_keys_derive_a_stable_address_hash() {
    let sender = identity_from(SENDER_KEY);

    assert_eq!(
        sender.as_identity().address_hash.to_hex_string(),
        SENDER_ADDRESS_HASH
    );
}

#[test]
fn identity_keys_round_trip_through_their_serialized_forms() {
    let private = identity_from(SENDER_KEY);
    let identity = private.as_identity();

    assert_eq!(private.to_private_key_bytes(), SENDER_KEY);

    let restored = Identity::new_from_hex_string(&identity.to_hex_string())
        .expect("an identity's own hex encoding is valid input");
    assert_eq!(restored.address_hash, identity.address_hash);
}

#[test]
fn identity_signatures_verify_and_reject_tampering() {
    let private = identity_from(SENDER_KEY);
    let identity = private.as_identity();

    let signature = lxmf_sign(&private, b"coyote");
    assert!(lxmf_verify(identity, b"coyote", &signature));
    assert!(!lxmf_verify(identity, b"coyot3", &signature));

    let other = identity_from(RECIPIENT_KEY);
    assert!(!lxmf_verify(other.as_identity(), b"coyote", &signature));
}

#[test]
fn lxmf_messages_round_trip_through_the_wire_format() {
    let sender = identity_from(SENDER_KEY);
    let recipient = identity_from(RECIPIENT_KEY);
    let wire = signed_frame(&sender, &recipient);

    let decoded = Message::from_wire(&wire).expect("what we just encoded decodes");
    assert_eq!(decoded.source_hash, Some(address_bytes(&sender)));
    assert_eq!(decoded.destination_hash, Some(address_bytes(&recipient)));
    assert_eq!(decoded.title_as_string().as_deref(), Some("smoke"));
    assert_eq!(decoded.content_as_string().as_deref(), Some("hello mesh"));

    let unpacked = WireMessage::unpack(&wire).expect("the wire frame unpacks");
    assert!(
        unpacked
            .verify(sender.as_identity())
            .expect("verification runs"),
        "the sender's signature must verify against the sender's identity"
    );
    assert!(
        !unpacked
            .verify(recipient.as_identity())
            .expect("verification runs"),
        "another identity's key must not verify the sender's signature"
    );
}

#[test]
fn lxmf_wire_frames_are_byte_stable() {
    let wire = signed_frame(&identity_from(SENDER_KEY), &identity_from(RECIPIENT_KEY));

    assert_eq!(hex_digest(&wire), WIRE_FRAME_DIGEST);
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn transport_storage_feature_is_enabled() {
    // Reaching this module at all requires the `storage` feature, and opening the
    // store requires the bundled SQLite to have compiled and linked.
    MessagesStore::in_memory().expect("an in-memory message store opens");
}
