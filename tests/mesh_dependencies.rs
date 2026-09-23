//! Smoke coverage for the Reticulum/LXMF dependencies.
//!
//! These crates enter the tree ahead of the code that uses them, so nothing else
//! would notice if the pinned revision stopped resolving, stopped exposing the
//! identity and wire surface, or lost the `storage` feature that brings bundled
//! SQLite in. The seeded vector below also pins key derivation itself: Reticulum
//! addresses peers by a truncated hash of their public keys, so a derivation
//! change is a protocol break, and it should fail here rather than on a live mesh.

use lxmf_core::identity::{Identity, PrivateIdentity, lxmf_sign, lxmf_verify};
use lxmf_core::{Message, WireMessage};
use rns_transport::storage::messages::MessagesStore;

const SEED: &str = "coyote-mesh-dependency-smoke";
const PEER_SEED: &str = "coyote-mesh-dependency-peer";

/// Address hash `SEED` derives to, captured from LXMF-rs at the pinned revision.
const SEED_ADDRESS_HASH: &str = "5f8e98bea1ceb371a4e1fec85843ad7b";

fn address_bytes(identity: &PrivateIdentity) -> [u8; 16] {
    identity
        .address_hash()
        .as_slice()
        .try_into()
        .expect("a Reticulum address hash is 16 bytes")
}

#[test]
fn seeded_identity_derives_a_stable_address_hash() {
    let private = PrivateIdentity::new_from_name(SEED);

    assert_eq!(
        private.as_identity().address_hash.to_hex_string(),
        SEED_ADDRESS_HASH
    );
}

#[test]
fn identity_keys_round_trip_through_their_serialized_forms() {
    let private = PrivateIdentity::new_from_name(SEED);
    let identity = private.as_identity();

    let restored_private = PrivateIdentity::from_private_key_bytes(&private.to_private_key_bytes())
        .expect("64 private key bytes are a valid identity");
    assert_eq!(restored_private.address_hash(), &identity.address_hash);

    let restored_public = Identity::new_from_hex_string(&identity.to_hex_string())
        .expect("an identity's own hex encoding is valid input");
    assert_eq!(restored_public.address_hash, identity.address_hash);
}

#[test]
fn identity_signatures_verify_and_reject_tampering() {
    let private = PrivateIdentity::new_from_name(SEED);
    let identity = private.as_identity();

    let signature = lxmf_sign(&private, b"coyote");
    assert!(lxmf_verify(identity, b"coyote", &signature));
    assert!(!lxmf_verify(identity, b"coyot3", &signature));

    let other = PrivateIdentity::new_from_name(PEER_SEED);
    assert!(!lxmf_verify(other.as_identity(), b"coyote", &signature));
}

#[test]
fn lxmf_messages_round_trip_through_the_wire_format() {
    let sender = PrivateIdentity::new_from_name(SEED);
    let recipient = PrivateIdentity::new_from_name(PEER_SEED);

    let mut message = Message::new();
    message.source_hash = Some(address_bytes(&sender));
    message.destination_hash = Some(address_bytes(&recipient));
    // Pinned so the encoding is reproducible; `to_wire` otherwise stamps "now".
    message.timestamp = Some(1_700_000_000.0);
    message.set_title_from_string("smoke");
    message.set_content_from_string("hello mesh");

    let wire = message
        .to_wire(Some(&sender))
        .expect("a signed message encodes");

    let decoded = Message::from_wire(&wire).expect("what we just encoded decodes");
    assert_eq!(decoded.source_hash, message.source_hash);
    assert_eq!(decoded.destination_hash, message.destination_hash);
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
fn transport_storage_feature_is_enabled() {
    // Reaching this module at all requires the `storage` feature, and opening the
    // store requires the bundled SQLite to have compiled and linked.
    MessagesStore::in_memory().expect("an in-memory message store opens");
}
