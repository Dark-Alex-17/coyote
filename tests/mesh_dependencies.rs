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
//! The manifest checks at the bottom pin what no compiler here can be asked
//! about: the Win32 feature list, which nothing on a unix host links against,
//! and the tracked records the pin and the license obligations rest on. The
//! three pin checks expire with the git pin; the Win32 and obligation checks
//! outlive it.

use lxmf_core::identity::{Identity, PrivateIdentity, lxmf_sign, lxmf_verify};
use lxmf_core::stamp::{COST_TICKET, TICKET_LENGTH, generate_stamp, ticket_stamp, validate_stamp};
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

/// Reads a tracked document with its hand-wrapping normalised away, since every
/// assertion below is about what the prose says, not how it is laid out.
fn read_prose(name: &str) -> String {
    squash(&read_tracked(name))
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

/// The stamp API is the whole reason the pin exists, so exercise it rather than
/// leaving the justification as prose in the manifest. The published release
/// carries the surrounding `stamp` module without these three calls, so a swap to
/// any release that still lacks them fails to compile here instead of silently
/// dropping the feature the mesh work is built on. Note the public path: the
/// `delivery` module they live in is private, and they are re-exported one level up.
#[test]
fn the_stamp_api_the_pin_exists_for_is_reachable() {
    let message_id = [0x33; 32];

    let stamp = generate_stamp(&message_id, 4).expect("a cost of 4 is mineable");
    assert!(
        validate_stamp(Some(&stamp), &message_id, 4, &[]).is_some_and(|value| value >= 4),
        "a freshly mined stamp must validate at the cost it was mined for"
    );

    let ticket = vec![0x44; TICKET_LENGTH];
    let from_ticket = ticket_stamp(&ticket, &message_id);
    assert_eq!(
        validate_stamp(Some(&from_ticket), &message_id, 4, &[ticket]),
        Some(COST_TICKET),
        "a stamp derived from a held ticket must be worth the ticket cost"
    );

    assert_eq!(
        validate_stamp(Some(&[0; 32]), &message_id, 16, &[]),
        None,
        "an all-zero stamp carries no work and must be rejected at a real cost"
    );
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

    // The manifest is what was declared; the lock is what resolved. Both crates plus
    // the `reticulum-rs-core` they share must have landed on the audited commit.
    let resolved = read_tracked("Cargo.lock")
        .matches(&format!("?rev={PINNED_REV}#{PINNED_REV}"))
        .count();
    assert_eq!(
        resolved, 3,
        "the lock must resolve all three LXMF-rs crates to {PINNED_REV}"
    );
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
         adding, dropping or reordering one needs the same audit the original list got"
    );
}

/// Every git pin is interim, so the manifest has to say so where someone retiring it
/// will look, and both copies of the gate have to stay runnable and identical: a
/// contributor ticks the checklist, not the guide.
#[test]
fn the_git_pin_is_recorded_as_interim() {
    // Comment markers and hand-wrapping are noise here; the sentence is the contract.
    let manifest_prose = squash(&read_tracked("Cargo.toml").replace("\n#", "\n"));
    assert!(
        manifest_prose.contains("crates.io version pin before merge"),
        "the manifest must record that the git pin cannot survive a merge"
    );

    let gate = squash(
        "meta=$(cargo metadata --format-version 1 --locked) \
         && ! printf '%s' \"$meta\" | grep -q '\"source\":\"git+'",
    );
    for document in [
        "CONTRIBUTING.md",
        ".github/PULL_REQUEST_TEMPLATE/pull_request_template.md",
    ] {
        assert!(
            read_prose(document).contains(&gate),
            "{document} must quote the no-git-sources gate verbatim; the checklist a \
             contributor ticks and the guide that explains it cannot drift apart"
        );
    }
}

/// Two maintained crates were passed over for raw bindings, and the reason is a
/// judgement that decays: dormancy and an old binding stack. Whoever revisits the
/// Win32 choice needs that record next to the dependency it justifies, so pin the
/// facts it rests on rather than only the conclusion.
#[test]
fn the_passed_over_win32_crates_stay_recorded() {
    let manifest = squash(&read_tracked("Cargo.toml"));

    for (crate_name, last_release) in [
        ("windows-acl", "2021-01-11"),
        ("windows-permissions", "2021-06-29"),
    ] {
        assert!(
            manifest.contains(crate_name),
            "the manifest must record why {crate_name} was passed over"
        );
        assert!(
            manifest.contains(last_release),
            "the record for {crate_name} must keep its last-release date, which is what \
             makes the dormancy claim checkable"
        );
    }

    assert!(
        manifest.contains("second Win32 binding stack"),
        "the record must keep the cost of adopting either one"
    );
}

/// These dependencies cost compile time, and the Windows figure cannot be taken from
/// a unix host. The measurement therefore lives in a tracked file with the Windows
/// row explicitly outstanding, so the gap stays visible instead of reading as zero.
#[test]
fn the_dependency_cost_record_is_tracked_and_names_the_windows_gap() {
    let guide = read_prose("CONTRIBUTING.md");

    assert!(
        guide.contains("cargo test --all`, test execution only"),
        "the full-suite wall clock must be recorded in the tracked guide"
    );
    assert!(
        guide.contains("windows-latest` CI leg"),
        "the record must carry a Windows row, the one figure a unix host cannot measure"
    );
    assert!(
        guide.contains("cannot be measured outside CI from a non-Windows host"),
        "the record must say why the Windows row is outstanding rather than leaving it blank"
    );
}

/// NOTICE is where the license obligations live, CREDITS.md summarises them and
/// CONTRIBUTING.md gates releases on them. The gate may block on a subset, but it
/// may not silently omit one, so all three have to agree on how many there are and
/// on which one blocks. Adding a fourth copy of this claim without wiring it in here
/// is how the three drifted apart before.
#[test]
fn the_release_gate_names_every_obligation_notice_records() {
    let notice = read_prose("NOTICE");
    assert!(
        notice.contains("EPL-2.0 OR GPL-2.0-or-later"),
        "NOTICE must record the dual license the mesh crates actually ship under"
    );
    // The spelled-out count, the bullets it counts and the single blocking marker all
    // have to move together, or the three documents drift the way they did before.
    assert!(
        notice.contains("Two obligations of Coyote's own"),
        "NOTICE must state how many outstanding obligations it records"
    );
    assert_eq!(
        read_tracked("NOTICE").matches("\n  - ").count(),
        2,
        "NOTICE's obligation bullets must match the count its prose states"
    );
    assert_eq!(
        notice.matches("This one blocks a release").count(),
        1,
        "exactly one recorded obligation is release-blocking; changing that has to be \
         mirrored in the CONTRIBUTING.md gate"
    );

    let credits = read_prose("CREDITS.md");
    assert!(
        credits.contains("Two obligations follow for Coyote"),
        "the CREDITS.md summary must agree with NOTICE on the count"
    );
    assert!(
        credits.contains("GPL-2.0-or-later for the mesh crates and BSD 3-Clause"),
        "the CREDITS.md summary must agree on the scope of the blocking obligation too, \
         not only on the count"
    );

    let gate = read_prose("CONTRIBUTING.md");
    for obligation in [
        "the license texts the distributed binary relies on",
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
