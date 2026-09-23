//! Smoke coverage for the Reticulum/LXMF dependencies.
//!
//! These crates enter the tree ahead of the code that uses them, so nothing else
//! would notice if the pinned revision stopped resolving, stopped exposing the
//! identity and wire surface, or lost the `storage` feature that brings bundled
//! SQLite in. The two captured vectors go further and pin the protocol itself:
//! Reticulum addresses peers by a truncated hash of their public keys, and LXMF
//! peers agree on an exact frame layout, so a change to either is an interop
//! break that should fail here rather than against a live Python node.
//!
//! The manifest checks at the bottom pin what the Windows leg cannot be asked
//! about from a unix host, and they expire with the git pin they describe.

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

/// The revision both mesh crates are pinned to. Anything that moves the pin has to
/// move it here too, which is the point: the vectors above are only meaningful
/// against a known revision.
const PINNED_REV: &str = "3ed5932da4420e2dd1b9d36283b0e72a364e3ebe";

/// The Win32 feature set, one entry per call the identity-key lockdown makes. An
/// addition here is unaudited surface and a removal breaks a call, so the list is
/// pinned exactly rather than by containment. Nothing on a unix host compiles
/// against these, which is why they are checked as manifest text.
const WINDOWS_SYS_FEATURES: [&str; 5] = [
    "Win32_Foundation",
    "Win32_Security",
    "Win32_Security_Authorization",
    "Win32_Storage_FileSystem",
    "Win32_System_Threading",
];

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

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_tracked(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{name} is readable"))
}

/// Returns the body of `[section]`, up to the next section header.
fn manifest_section<'a>(manifest: &'a str, section: &str) -> &'a str {
    let header = format!("[{section}]");
    let start = manifest
        .find(&header)
        .unwrap_or_else(|| panic!("the manifest has a {header} section"))
        + header.len();
    let rest = &manifest[start..];
    match rest.find("\n[") {
        Some(end) => &rest[..end],
        None => rest,
    }
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

#[test]
fn transport_storage_feature_is_enabled() {
    // Reaching this module at all requires the `storage` feature, and opening the
    // store requires the bundled SQLite to have compiled and linked.
    MessagesStore::in_memory().expect("an in-memory message store opens");
}

#[test]
fn mesh_crates_are_pinned_to_the_audited_revision() {
    let manifest = read_tracked("Cargo.toml");
    let deps = manifest_section(&manifest, "dependencies");

    for crate_name in ["reticulum-rs-transport", "lxmf-wire"] {
        let line = deps
            .lines()
            .find(|line| line.starts_with(&format!("{crate_name} =")))
            .unwrap_or_else(|| panic!("{crate_name} is declared in [dependencies]"));
        assert!(
            line.contains(&format!("rev = \"{PINNED_REV}\"")),
            "{crate_name} must stay pinned to {PINNED_REV}, found: {line}"
        );
    }
}

/// The mesh crates carry no `cfg` gate, so a break shows up on every platform's CI
/// leg rather than on whichever one happens to be gated in.
#[test]
fn mesh_crates_are_not_target_scoped() {
    let manifest = read_tracked("Cargo.toml");

    for crate_name in ["reticulum-rs-transport", "lxmf-wire"] {
        let declaration = format!("\n{crate_name} =");
        let at = manifest
            .find(&declaration)
            .unwrap_or_else(|| panic!("{crate_name} is declared"));
        let enclosing = manifest[..at]
            .rmatch_indices("\n[")
            .next()
            .map(|(start, _)| manifest[start + 1..].lines().next().unwrap_or_default())
            .expect("a dependency sits inside some section");
        assert_eq!(
            enclosing, "[dependencies]",
            "{crate_name} must be an unconditional dependency, found it under {enclosing}"
        );
    }
}

#[test]
fn windows_sys_carries_exactly_the_audited_feature_set() {
    let manifest = read_tracked("Cargo.toml");
    let section = manifest_section(&manifest, "target.'cfg(windows)'.dependencies");
    assert!(
        section.contains("windows-sys = {"),
        "windows-sys must be declared under cfg(windows), not unconditionally"
    );

    let features: Vec<&str> = section
        .lines()
        .filter_map(|line| {
            line.trim()
                .trim_end_matches(',')
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .collect();

    assert_eq!(
        features, WINDOWS_SYS_FEATURES,
        "the windows-sys feature list changed; each entry backs a named Win32 call, so \
         adding or dropping one needs the same audit the original list got"
    );
}

/// Every git pin is interim, so the manifest has to say so where someone retiring it
/// will look, and the contributor guide has to carry the runnable gate itself.
#[test]
fn the_git_pin_is_recorded_as_interim() {
    assert!(
        read_tracked("Cargo.toml").contains("crates.io version pin before merge"),
        "the manifest must record that the git pin cannot survive a merge"
    );

    assert!(
        read_tracked("CONTRIBUTING.md")
            .contains("cargo metadata --format-version 1 | grep '\"source\":\"git+'"),
        "the contributor guide must carry the runnable no-git-sources gate"
    );
}

/// NOTICE is where the license obligations live and CONTRIBUTING.md is where the
/// release gate reads them back. The gate is allowed to block on a subset, but it
/// may not silently omit one, so it has to name every obligation NOTICE records.
#[test]
fn the_release_gate_names_every_obligation_notice_records() {
    let notice = read_tracked("NOTICE");
    assert!(
        notice.contains("EPL-2.0 OR GPL-2.0-or-later"),
        "NOTICE must record the dual license the mesh crates actually ship under"
    );

    let gate = squash(&read_tracked("CONTRIBUTING.md"));
    for obligation in [
        "a copy of that license text is not yet in this repository",
        "settling the license expression for the combined work",
    ] {
        assert!(
            gate.contains(obligation),
            "the release gate in CONTRIBUTING.md must account for the NOTICE obligation \
             {obligation:?}, so a recorded obligation cannot slip through unmentioned"
        );
    }
}

/// Markdown wraps prose across lines, so compare against a single-spaced copy.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
