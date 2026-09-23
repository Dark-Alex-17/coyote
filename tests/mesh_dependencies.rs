//! Smoke coverage for the Reticulum/LXMF dependencies.
//!
//! These crates enter the tree ahead of the code that uses them, so nothing else
//! would notice if the pinned revision stopped resolving, stopped exposing the
//! identity surface, or lost the `storage` feature that brings bundled SQLite in.
//! The seeded vector below also pins key derivation itself: Reticulum addresses
//! peers by a truncated hash of their public keys, so a derivation change is a
//! protocol break, and it should fail here rather than on a live mesh.

use lxmf_core::identity::{Identity, PrivateIdentity, lxmf_sign, lxmf_verify};
use rns_transport::storage::messages::MessagesStore;

const SEED: &str = "coyote-mesh-dependency-smoke";

/// Address hash `SEED` derives to, captured from LXMF-rs at the pinned revision.
const SEED_ADDRESS_HASH: &str = "5f8e98bea1ceb371a4e1fec85843ad7b";

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn seeded_identity_derives_a_stable_address_hash() {
    let private = PrivateIdentity::new_from_name(SEED);
    let identity = private.as_identity();

    assert_eq!(identity.address_hash.as_slice().len(), 16);
    assert_eq!(to_hex(identity.address_hash.as_slice()), SEED_ADDRESS_HASH);
    assert_eq!(private.address_hash(), &identity.address_hash);
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

    let other = PrivateIdentity::new_from_name("someone-else");
    assert!(!lxmf_verify(other.as_identity(), b"coyote", &signature));
}

#[test]
fn transport_storage_feature_is_enabled() {
    // Reaching this module at all requires the `storage` feature, and opening the
    // store requires the bundled SQLite to have compiled and linked.
    MessagesStore::in_memory().expect("an in-memory message store opens");
}
