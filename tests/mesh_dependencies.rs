//! Smoke coverage for the Reticulum/LXMF dependencies.
//!
//! These crates enter the tree ahead of the code that uses them, so nothing else
//! would notice if a release stopped exposing the identity, wire and stamp
//! surface, or lost the `storage` feature that brings bundled SQLite in. The two
//! captured vectors go further and pin the protocol itself: Reticulum addresses
//! peers by a truncated hash of their public keys, and LXMF peers agree on an
//! exact frame layout, so a change to either is an interop break that should fail
//! here rather than against a live Python node.
//!
//! The manifest checks at the bottom pin what no compiler on a unix host can be
//! asked about: the Win32 feature list, whose call sites nothing here links
//! against; the no-git-sources gate, whose enforcing step lives in a workflow
//! file; and the tracked records the dependency cost and the license obligations
//! rest on. Note what the feature-list check is and is not. It compares manifest
//! text, so it catches an unaudited addition or a silent drop; it cannot tell
//! whether windows-sys exposes those features, and does not need to: cargo
//! refuses to resolve a feature that does not exist upstream, so no build on any
//! platform reaches this test carrying one. The feature-to-call mapping is
//! checked by `the_audited_win32_features_expose_the_calls_they_are_carried_for`,
//! which only compiles on Windows.

use lxmf_core::identity::{Identity, PrivateIdentity, lxmf_sign, lxmf_verify};
use lxmf_core::stamp::{COST_TICKET, TICKET_LENGTH, generate_stamp, ticket_stamp, validate_stamp};
use lxmf_core::{Message, WireMessage};
use rns_transport::storage::messages::MessagesStore;
use sha2::{Digest, Sha256};

/// Fixed X25519 secret and Ed25519 signing key, 32 bytes each. Chosen arbitrarily;
/// what matters is that they never change, so the vectors below stay comparable.
const SENDER_KEY: [u8; 64] = [0x11; 64];
const RECIPIENT_KEY: [u8; 64] = [0x22; 64];

/// Address hash `SENDER_KEY` derives to, captured from LXMF-rs at rev 3ed5932 and
/// unchanged at release 0.12.0. Pins the public-key to address-hash derivation,
/// which is protocol.
const SENDER_ADDRESS_HASH: &str = "ef330a1940c70349459fc4401d273cb9";

/// SHA-256 of the packed frame `lxmf_wire_frames_are_byte_stable` builds. The
/// frame is deterministic: fixed keys, a fixed timestamp, no fields, and Ed25519
/// signing is deterministic, so any change to the encoding shows up here.
/// Captured at rev 3ed5932 and unchanged at release 0.12.0.
const WIRE_FRAME_DIGEST: &str = "9a7be88510c749b17fb240822805e30077412d1ec9a6fd25908ad29329460eed";

/// Timestamp baked into the frame vector. Held constant so the encoding is.
const FIXED_TIMESTAMP: f64 = 1_700_000_000.0;

/// The Win32 feature set, one entry per call the identity-key lockdown makes. An
/// addition here is unaudited surface and a removal breaks a call, so the list is
/// pinned exactly rather than by containment. Nothing on a unix host compiles
/// against these, which is why they are checked as manifest text.
const WINDOWS_SYS_FEATURES: [&str; 6] = [
    "Win32_Foundation",
    "Win32_Security",
    "Win32_Security_Authorization",
    "Win32_Storage_FileSystem",
    "Win32_System_SystemServices",
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

/// Reads a tracked document with its hand-wrapping normalised away. Most assertions
/// below are about what a document says rather than how it is wrapped; the obligation
/// bullet count is the exception and reads the raw text on purpose.
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

/// Returns the body of the top-level `merge-gates` job, up to the next job.
fn merge_gates_job(workflow: &str) -> &str {
    let header = "\n  merge-gates:\n";
    let start = workflow
        .find(header)
        .expect("ci.yaml has a merge-gates job")
        + header.len();
    let rest = &workflow[start..];
    let end = rest
        .match_indices("\n  ")
        .map(|(at, _)| at)
        .find(|&at| rest[at + 3..].starts_with(|c: char| c != ' ' && c != '\n'))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Returns the `run:` script of the `merge-gates` step called `name`. Raw rather than
/// squashed: the gate is a shell block, and its structure is what the caller checks.
fn merge_gate_script<'a>(workflow: &'a str, name: &str) -> &'a str {
    let job = merge_gates_job(workflow);
    let header = format!("    - name: {name}\n");
    let start = job
        .find(&header)
        .unwrap_or_else(|| panic!("the merge-gates job has a step named {name:?}"))
        + header.len();
    let step = &job[start..];
    let step = &step[..step.find("\n    - ").unwrap_or(step.len())];
    let run = "run: |\n";
    let start = step
        .find(run)
        .unwrap_or_else(|| panic!("the {name:?} step has a literal run block"))
        + run.len();
    &step[start..]
}

/// Returns the lines between the `if … ; then` line `guard` and the `fi` that closes it
/// at the same indentation.
fn then_branch<'a>(script: &'a str, guard: &str) -> Vec<&'a str> {
    let indent = &guard[..guard.len() - guard.trim_start().len()];
    let close = format!("{indent}fi");
    let after_guard = script
        .find(guard)
        .expect("the guard is a line of the script")
        + guard.len();
    let lines: Vec<&str> = script[after_guard..].lines().skip(1).collect();
    let fi = lines
        .iter()
        .position(|line| *line == close)
        .expect("the guard is closed by a fi at the same indentation");
    lines[..fi].to_vec()
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

/// The delivery-stamp API is what the mesh work is built on and what once forced a
/// git pin: releases before 0.12.0 carry the surrounding `stamp` module without
/// these three calls. Exercising them keeps a move to any release that lacks them
/// a compile failure here rather than a silently dropped feature. Note the public
/// path: the `delivery` module they live in is private, and they are re-exported
/// one level up.
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

/// The two mesh crates share `reticulum-rs-core`, and cargo unifies a shared dependency
/// only within one semver range: resolving them at incompatible versions duplicates the
/// core, so an identity from one half is a foreign type to the other. The manifest
/// promises they move together; this holds it to that promise, and to the single core
/// the promise exists for.
#[test]
fn the_mesh_crates_move_together() {
    let manifest = read_tracked("Cargo.toml");
    let dependencies = manifest_section(&manifest, "dependencies");

    let version_of = |crate_name: &str| -> &str {
        let declaration = format!("{crate_name} = ");
        dependencies
            .lines()
            .find_map(|line| line.strip_prefix(&declaration))
            .unwrap_or_else(|| panic!("{crate_name} is declared under [dependencies]"))
            .trim()
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or_else(|| panic!("{crate_name} is pinned to a plain version string"))
    };
    assert_eq!(
        version_of("reticulum-rs-transport"),
        version_of("lxmf-wire"),
        "reticulum-rs-transport and lxmf-wire must be pinned to the same release"
    );

    let lock = read_tracked("Cargo.lock");
    assert_eq!(
        lock.lines()
            .filter(|line| *line == "name = \"reticulum-rs-core\"")
            .count(),
        1,
        "the resolved graph must carry exactly one reticulum-rs-core"
    );
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

/// Naming each audited feature's call turns the list above from manifest text into a
/// compile-time check of the claim the list is making: that each feature is carried
/// for one named Win32 item, and that the item is where the manifest says it is.
/// Nothing is invoked; taking an address is enough to require the import to resolve
/// (through a pointer first: casting a function item straight to an integer trips rustc's
/// `function_casts_as_integer` lint, warn-by-default since 1.98, which this leg's
/// `--deny warnings` turns into an error).
/// Only the windows-latest CI leg compiles this.
#[cfg(windows)]
#[test]
fn the_audited_win32_features_expose_the_calls_they_are_carried_for() {
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::ACL;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetVolumeInformationByHandleW};
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_ALLOWED_ACE_TYPE, FILE_PERSISTENT_ACLS,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken};

    let audited_calls: [usize; 9] = [
        ConvertStringSecurityDescriptorToSecurityDescriptorW as *const () as usize,
        GetSecurityInfo as *const () as usize,
        ConvertSidToStringSidW as *const () as usize,
        LocalFree as *const () as usize,
        CreateFileW as *const () as usize,
        GetVolumeInformationByHandleW as *const () as usize,
        OpenProcessToken as *const () as usize,
        GetCurrentProcess as *const () as usize,
        OpenProcess as *const () as usize,
    ];
    assert!(
        audited_calls.iter().all(|address| *address != 0),
        "every audited Win32 call must resolve to a real import"
    );

    assert!(
        core::mem::size_of::<ACL>() > 0,
        "Win32_Security is carried for the ACL type"
    );
    assert_eq!(
        core::mem::size_of::<HLOCAL>(),
        core::mem::size_of::<*mut core::ffi::c_void>(),
        "Win32_Foundation is carried for the LocalFree/HLOCAL pair"
    );
    assert_eq!(
        FILE_PERSISTENT_ACLS, 8,
        "Win32_System_SystemServices is carried for the volume flag that tells an \
         ACL-keeping volume from FAT"
    );
    assert_eq!(
        ACCESS_ALLOWED_ACE_TYPE, 0,
        "Win32_System_SystemServices is carried for the ACE type the DACL walk compares \
         each header against"
    );
}

/// Nothing merges with a git source in its resolved graph. The human-facing copy of
/// that gate has to sit at the path GitHub auto-populates a pull request body from,
/// and stay identical to the guide's copy; the `.github/PULL_REQUEST_TEMPLATE/`
/// directory form renders only for an explicit `?template=` link, so a checklist
/// there is a checklist nobody is shown. The gate that actually holds is the CI job,
/// which is why its mechanism is pinned here too.
#[test]
fn the_no_git_sources_gate_is_quoted_and_enforced() {
    let gate = squash(
        "meta=$(cargo metadata --format-version 1 --locked) \
         && ! printf '%s' \"$meta\" | grep -q '\"source\":\"git+'",
    );
    for document in ["CONTRIBUTING.md", ".github/pull_request_template.md"] {
        assert!(
            read_prose(document).contains(&gate),
            "{document} must quote the no-git-sources gate verbatim; the checklist a \
             contributor ticks and the guide that explains it cannot drift apart"
        );
    }

    // The checklist is a courtesy; this is the gate. Pinned by mechanism rather than
    // by wording: the step has to resolve the locked graph, test it for a git source,
    // and fail the job inside that test's branch, however the messages around it read.
    // The helpers that walk the script match line structure on bare `\n`, so a CRLF
    // checkout (the windows-latest leg) is normalised first; the squashed reads need not be.
    let workflow = read_tracked(".github/workflows/ci.yaml").replace("\r\n", "\n");
    let script = merge_gate_script(&workflow, "Nothing is pulled from git");
    assert!(
        script.contains("cargo metadata --format-version 1 --locked"),
        "the merge gate must resolve the locked graph itself rather than trust the manifest"
    );
    let guard = script
        .lines()
        .find(|line| {
            let line = line.trim();
            line.starts_with("if ")
                && line.contains("grep -q '\"source\":\"git+'")
                && line.ends_with("; then")
        })
        .expect("the merge gate tests the resolved graph for a git source in an if … ; then");
    assert!(
        then_branch(script, guard)
            .iter()
            .any(|line| line.trim() == "exit 1"),
        "the merge gate must `exit 1` inside the branch that found a git source; a message \
         without the exit, or an exit outside the branch, leaves the job green or always red"
    );
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
/// a unix host, so it was read off the first green `windows-latest` CI run and written
/// into the tracked guide. The row used to carry `TBD`; a placeholder there would now
/// mean the record regressed, so this pins the filled figures, the verdict the task
/// required, and the three-OS green result.
#[test]
fn the_dependency_cost_record_carries_the_measured_windows_figures_and_a_verdict() {
    let raw = read_tracked("CONTRIBUTING.md");
    let guide = squash(&raw);

    assert!(
        guide.contains("cargo test --all`, test execution only"),
        "the full-suite wall clock must be recorded in the tracked guide"
    );

    let windows_rows: Vec<&str> = raw
        .lines()
        .filter(|line| line.starts_with("| `windows-latest` CI leg"))
        .collect();
    assert_eq!(
        windows_rows.len(),
        2,
        "the build-cost table must carry the job-duration row and the Test-step row for \
         windows-latest; found {windows_rows:?}"
    );
    for row in &windows_rows {
        assert!(
            !row.contains("TBD") && !row.contains("not measured"),
            "the windows-latest rows were filled from CI and must not regress to a \
             placeholder; found {row:?}"
        );
    }
    assert!(
        windows_rows[0].contains("1h02m41s") && windows_rows[1].contains("3m13s"),
        "the windows-latest rows must carry the figures read off the first green run"
    );

    assert!(
        guide.contains("Verdict: acceptable"),
        "the record must state whether the Windows cost is acceptable, not only what it is"
    );
    assert!(
        guide.contains("green under `-D warnings` on all three OSes"),
        "the record must carry the three-OS green result for `cargo build` and `cargo test --all`"
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
    // Counted over the obligations paragraph alone, on the raw text: squashing would
    // destroy the indentation that makes a bullet a bullet, and counting file-wide
    // would answer a different question than the one the prose states.
    let raw = read_tracked("NOTICE");
    let obligations = raw
        .split_once("Two obligations of Coyote's own")
        .and_then(|(_, rest)| rest.split_once("Transitive dependencies."))
        .map(|(block, _)| block)
        .expect("NOTICE states its obligations between that sentence and the next section");
    assert_eq!(
        obligations.matches("\n  - ").count(),
        2,
        "NOTICE's obligation bullets must match the count its prose states"
    );
    assert_eq!(
        notice.matches("This one blocks a release").count(),
        1,
        "exactly one recorded obligation is release-blocking; changing that has to be \
         mirrored in the CONTRIBUTING.md gate"
    );

    assert!(
        read_prose("CREDITS.md").contains("Two obligations follow for Coyote"),
        "the CREDITS.md summary must agree with NOTICE on the count"
    );
    // Scope, not just count: narrowing the blocking obligation to one of the two
    // license texts in either document is the drift this catches.
    for document in ["CREDITS.md", "CONTRIBUTING.md"] {
        assert!(
            read_prose(document).contains("GPL-2.0-or-later for the mesh crates and BSD 3-Clause"),
            "{document} must agree on the scope of the blocking obligation, not only on \
             the count"
        );
    }

    let gate = read_prose("CONTRIBUTING.md");
    // The shortest phrase that still identifies each obligation: rewording the prose
    // around them is not drift, dropping one from the gate is.
    for obligation in [
        "the license texts",
        "license expression for the combined work",
    ] {
        assert!(
            gate.contains(obligation),
            "the release gate in CONTRIBUTING.md must account for the NOTICE obligation \
             {obligation:?}, so a recorded obligation cannot slip through unmentioned"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests for the crates.io pin. Each holds one promise it makes about the tree
// as a consumer meets it: `cargo build --locked` pulls nothing from git, the
// release is the one the stamp API needs, the guide no longer describes a git
// pin that is gone, and the merge gate's mechanism check can actually go red.
// ---------------------------------------------------------------------------

/// The version string both mesh crates are declared at under `[dependencies]`.
fn declared_mesh_crate_version(manifest: &str, crate_name: &str) -> String {
    let dependencies = manifest_section(manifest, "dependencies");
    let declaration = format!("{crate_name} = ");
    dependencies
        .lines()
        .find_map(|line| line.strip_prefix(&declaration))
        .unwrap_or_else(|| panic!("{crate_name} is declared under [dependencies]"))
        .trim()
        .to_string()
}

/// Returns the `[[package]]` block of `crate_name` in the lockfile, as its lines.
fn locked_package<'a>(lock: &'a str, crate_name: &str) -> Vec<&'a str> {
    let name_line = format!("name = \"{crate_name}\"");
    let lines: Vec<&str> = lock.lines().collect();
    let at = lines
        .iter()
        .position(|line| *line == name_line)
        .unwrap_or_else(|| panic!("Cargo.lock resolves {crate_name}"));
    let end = lines[at..]
        .iter()
        .position(|line| *line == "[[package]]")
        .map(|offset| at + offset)
        .unwrap_or(lines.len());
    lines[at..end].to_vec()
}

fn semver_triple(version: &str) -> (u64, u64, u64) {
    let mut parts = version.split('.').map(|part| {
        part.parse::<u64>()
            .unwrap_or_else(|_| panic!("{version:?} is a plain MAJOR.MINOR.PATCH release"))
    });
    let triple = (
        parts.next().expect("major"),
        parts.next().expect("minor"),
        parts.next().expect("patch"),
    );
    assert!(
        parts.next().is_none(),
        "{version:?} has exactly three components"
    );
    triple
}

/// No git source and the registry release, as `cargo build --locked` sees them: the
/// resolved graph the lockfile records carries no git source at all, and the three
/// LXMF-rs crates it resolves (the two declared ones plus the `reticulum-rs-core` they
/// share) all come from the registry at exactly the version the manifest declares. The
/// CI merge gate asks `cargo metadata` the same question; this asks the tracked file, so
/// the answer is available on every platform's test leg without a network.
#[test]
fn usage_probe_the_lockfile_resolves_the_mesh_crates_from_the_registry_with_no_git_source() {
    let lock = read_tracked("Cargo.lock");
    let git_sources: Vec<&str> = lock
        .lines()
        .filter(|line| line.starts_with("source = \"git+"))
        .collect();
    assert!(
        git_sources.is_empty(),
        "Cargo.lock must record no git source; found {git_sources:?}"
    );

    let manifest = read_tracked("Cargo.toml");
    let declared = declared_mesh_crate_version(&manifest, "reticulum-rs-transport")
        .trim_matches('"')
        .to_string();
    for crate_name in ["reticulum-rs-transport", "lxmf-wire", "reticulum-rs-core"] {
        let package = locked_package(&lock, crate_name);
        assert!(
            package.contains(&format!("version = \"{declared}\"").as_str()),
            "{crate_name} must resolve to the declared release {declared}; block was {package:?}"
        );
        assert!(
            package
                .iter()
                .any(|line| line.starts_with("source = \"registry+")),
            "{crate_name} must resolve from the registry, not a git checkout; block was {package:?}"
        );
        assert!(
            package.iter().any(|line| line.starts_with("checksum = \"")),
            "a registry-sourced {crate_name} carries a checksum, which is what makes the pin \
             reproducible"
        );
    }
}

/// The release and the pin form: a release strictly newer than 0.11.0, because
/// 0.11.0 is what the retired git revision also called itself and is the release that
/// lacks the delivery-stamp calls; and a plain caret string, the repo convention, rather
/// than an `=` requirement or an inline table. `the_mesh_crates_move_together` already
/// holds the two crates to the same string; this holds that string to the right range.
#[test]
fn usage_probe_the_mesh_pin_is_a_plain_release_strictly_newer_than_0_11_0() {
    let manifest = read_tracked("Cargo.toml");

    for crate_name in ["reticulum-rs-transport", "lxmf-wire"] {
        let declared = declared_mesh_crate_version(&manifest, crate_name);
        let version = declared
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or_else(|| {
                panic!("{crate_name} must be a plain quoted version string, found {declared}")
            });
        assert!(
            !version.starts_with(['=', '^', '~', '>', '<', '*']),
            "{crate_name} must use the bare caret form the repo uses everywhere else, found \
             {version:?}"
        );
        assert!(
            semver_triple(version) > (0, 11, 0),
            "{crate_name} must be strictly newer than 0.11.0, the release without the \
             delivery-stamp API; found {version}"
        );
    }
}

/// The guide drops the retired pin: the subsection that described the interim git pin
/// goes with the pin, and with it every mention of the retired revision in the guide,
/// while the dependency policy and the two kept records stay where a contributor looks
/// for them, Windows row included.
#[test]
fn usage_probe_contributing_retired_the_interim_pin_prose_and_kept_the_records() {
    let raw = read_tracked("CONTRIBUTING.md");
    let guide = squash(&raw);

    for retired in ["The in-flight mesh pins", "3ed5932", "rev = \""] {
        assert!(
            !guide.contains(retired),
            "CONTRIBUTING.md must not describe the retired git pin; found {retired:?}"
        );
    }

    for kept in [
        "## Dependency policy",
        "### No git dependencies at merge",
        "### Dependency build-cost records",
        "### What was checked for the windows-sys feature list, and what was not",
    ] {
        assert!(
            raw.lines().any(|line| line.trim_end() == kept),
            "CONTRIBUTING.md must keep the section {kept:?}"
        );
    }
    assert!(
        raw.lines()
            .any(|line| { line.starts_with("| `windows-latest` CI leg") && !line.contains("TBD") }),
        "the build-cost table must keep its windows-latest row, filled from CI"
    );
}

/// Red-capability: the mechanism assertion in
/// `the_no_git_sources_gate_is_quoted_and_enforced` is only worth having if a neutered
/// step fails it. Feed the same helpers the tracked workflow with its `exit 1` removed,
/// and again with the exit moved outside the `if … fi`, and check that neither variant
/// still shows an exit inside the branch that found a git source. The tracked file is
/// never modified; the variants are in-memory copies.
#[test]
fn usage_probe_the_gate_mechanism_check_goes_red_when_the_exit_is_neutered() {
    let workflow = read_tracked(".github/workflows/ci.yaml").replace("\r\n", "\n");
    let script = merge_gate_script(&workflow, "Nothing is pulled from git");
    let guard = script
        .lines()
        .find(|line| line.trim().starts_with("if ") && line.trim().ends_with("; then"))
        .expect("the gate has an if … ; then guard");
    let exit_line = script
        .lines()
        .find(|line| line.trim() == "exit 1")
        .expect("the tracked gate carries an exit 1");
    let indent = &exit_line[..exit_line.len() - exit_line.trim_start().len()];
    let fi_line = format!("{}fi", &guard[..guard.len() - guard.trim_start().len()]);

    // Positive control: the tracked workflow passes the same check the variants fail.
    assert!(
        then_branch(script, guard)
            .iter()
            .any(|line| line.trim() == "exit 1"),
        "the tracked gate exits inside its branch (control)"
    );

    let without_exit = workflow.replacen(&format!("{exit_line}\n"), "", 1);
    let script = merge_gate_script(&without_exit, "Nothing is pulled from git");
    assert!(
        !then_branch(script, guard)
            .iter()
            .any(|line| line.trim() == "exit 1"),
        "a gate whose exit was deleted must fail the mechanism check"
    );

    let exit_after_fi = without_exit.replacen(
        &format!("{fi_line}\n"),
        &format!("{fi_line}\n{indent}exit 1\n"),
        1,
    );
    let script = merge_gate_script(&exit_after_fi, "Nothing is pulled from git");
    assert!(
        script.lines().any(|line| line.trim() == "exit 1"),
        "the variant still carries an exit 1, just outside the branch (control)"
    );
    assert!(
        !then_branch(script, guard)
            .iter()
            .any(|line| line.trim() == "exit 1"),
        "an exit outside the if … fi (a job that is always red) must fail the mechanism check"
    );
}

/// Markdown wraps prose across lines, so compare against a single-spaced copy.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
