//! Pins the README's mesh section, the constants the code uses and the source-comment
//! hygiene rules to the code, plus the envoy session-key and fetch response-bound vectors
//! and the rule that mesh text is printed only through its sinks. The scanners are
//! line-oriented and std-only.

use std::path::{Path, PathBuf};

const README: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));

const SCOPE_NAME: &str = "SCOPE: Session Coordination & Presence Exchange";
const SCOPE_TAGLINE: &str = "SCOPE is a peer protocol by which running LLM sessions announce presence, share status, and exchange messages on their owners' behalf, over Reticulum, without a broker.";

/// Splits `line` into (prose, code span) pairs; the last pair's span is empty.
fn split_spans(line: &str) -> Vec<(&str, &str)> {
    let mut parts = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`') else {
            break;
        };
        parts.push((&rest[..open], &rest[open + 1..open + 1 + close]));
        rest = &rest[open + close + 2..];
    }
    parts.push((rest, ""));
    parts
}

fn strip_spans(line: &str) -> String {
    let parts = split_spans(line);
    let mut out = String::with_capacity(line.len());
    for (index, (prose, _)) in parts.iter().enumerate() {
        out.push_str(prose);
        if index + 1 < parts.len() {
            out.push(' ');
        }
    }
    out
}

fn is_word_boundary(byte: Option<&u8>) -> bool {
    byte.is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
}

/// Every `.rs` file under `dir`, this file excepted: its fixtures name identifiers that exist
/// nowhere else, and the scanners below must not read this file's own fixtures.
fn rust_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let guard = Path::new(file!()).file_name();
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") && path.file_name() != guard {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Each `.rs` file under `root/src` as its `/`-joined path relative to `root` and its
/// contents, as `rust_files` walks it.
fn rust_source_files(root: &Path) -> std::io::Result<Vec<(String, String)>> {
    rust_files_under(root, "src")
}

/// Each `.rs` file under `root/<dir>` as its `/`-joined path relative to `root` and its
/// contents, as `rust_files` walks it.
fn rust_files_under(root: &Path, dir: &str) -> std::io::Result<Vec<(String, String)>> {
    rust_files(&root.join(dir))?
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            Ok((relative, std::fs::read_to_string(&path)?))
        })
        .collect()
}

// Redundant under a test-only module, but `request/tests.rs` splits production code from tests
// at exactly this marker when it counts the sites that name a refusal code.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mesh_config::{
        DEFAULT_ENVOY_MEMORY_MAX_BYTES, DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY,
        DEFAULT_ENVOY_MEMORY_MAX_SESSIONS, DEFAULT_ENVOY_MEMORY_MAX_TURNS,
        DEFAULT_ENVOY_MEMORY_TTL_HOURS, DEFAULT_FETCH_MAX_BYTES, DEFAULT_INLINE_MAX_BYTES,
        DEFAULT_PEER_MAX_CONCURRENT, DEFAULT_PEER_MAX_MESSAGES_PER_HOUR,
        DEFAULT_PEER_MAX_TOKENS_PER_HOUR, MAX_FETCH_FILE_BYTES, MAX_INLINE_FILE_TOTAL,
    };
    use crate::config::mesh_envoy::ENVOY_RUN_TIMEOUT_SECS;
    use crate::function::mesh::FETCH_INLINE_TEXT_MAX_BYTES;
    use crate::mesh::message::{MAX_PARTS, MAX_PARTS_BYTES};
    use crate::mesh::request::{MAX_FETCH_RESPONSE_BYTES, RefusalCode, RequestId, ResponseFrame};
    use crate::mesh::{
        access, announce, card, envoy_sessions, fetch, grants, identity, knock, knocks, limits,
        message, node, peers, pending, propagation, propagation_fetch, propagation_nodes, protocol,
        request, shares, trust, wire_path,
    };
    use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
    use rmpv::Value;
    use rns_transport::hash::ADDRESS_HASH_SIZE;
    use rns_transport::resource::MAX_EFFICIENT_SIZE;
    use std::collections::HashSet;
    use std::time::Duration;

    const EXPECTED_LITERALS: &str = r#"1,1,10,16,262144,"/knock","/status","/message",30,10,20,16,0xf0,0xf1,0xf3,0xf4,0xf5,0xf6,0xfd,0xfe,"SCOPE",64,300,3600,60,3660,3,5,10980,7320,1024,"scope.knock/1",200,15,10,256,3,600,16,256,16,1,0,1,2,64,280,64,64,280,"scope.peer/1",1,120,4000,64,4096,8,15,10,604800,256,3600,120,256,1,60,100000,120,26,60,2,60,1024,64,240,131072,112,4096,15552000,4096,86400,3,3660,256,0,32,86400,0xfb,0xfc,8,64,256,64,8,2,2,2,2,1,2,1,1,8,106496,98304,65536,"/list","/fetch",1024,64,1000,200,100000,64,2048,120,4194304,4194304,4198400,200,16,32,"/access","scope.access/1",16,500,5,900,1,1,1,16,32768,1048575,4096,1,1,256,16,40,65536,168"#;

    fn expected_constants() -> Vec<(&'static str, String)> {
        let secs = |d: Duration| d.as_secs().to_string();
        let quoted = |s: &str| format!("{s:?}");
        let byte = |v: u8| format!("0x{v:02x}");
        let code = |c: RefusalCode| byte(c as u8);
        vec![
            (
                "MESH_PROTOCOL_VERSION",
                protocol::MESH_PROTOCOL_VERSION.to_string(),
            ),
            (
                "MESH_PROTOCOL_MIN_SUPPORTED",
                protocol::MESH_PROTOCOL_MIN_SUPPORTED.to_string(),
            ),
            ("NAME_HASH_LEN", request::NAME_HASH_LEN.to_string()),
            ("ADDRESS_HASH_SIZE", ADDRESS_HASH_SIZE.to_string()),
            (
                "MAX_REQUEST_PAYLOAD_BYTES",
                request::MAX_REQUEST_PAYLOAD_BYTES.to_string(),
            ),
            ("KNOCK_PATH", quoted(request::KNOCK_PATH)),
            ("STATUS_PATH", quoted(request::STATUS_PATH)),
            ("MESSAGE_PATH", quoted(request::MESSAGE_PATH)),
            (
                "DEFAULT_REQUEST_TIMEOUT",
                secs(request::DEFAULT_REQUEST_TIMEOUT),
            ),
            ("DEFAULT_LINK_TIMEOUT", secs(request::DEFAULT_LINK_TIMEOUT)),
            ("HANDLER_TIMEOUT", secs(request::HANDLER_TIMEOUT)),
            (
                "MAX_CONCURRENT_INBOUND_REQUESTS",
                request::MAX_CONCURRENT_INBOUND_REQUESTS.to_string(),
            ),
            ("RefusalCode::NoIdentity", code(RefusalCode::NoIdentity)),
            ("RefusalCode::NoAccess", code(RefusalCode::NoAccess)),
            ("RefusalCode::InvalidKey", code(RefusalCode::InvalidKey)),
            ("RefusalCode::InvalidData", code(RefusalCode::InvalidData)),
            ("RefusalCode::InvalidStamp", code(RefusalCode::InvalidStamp)),
            ("RefusalCode::Throttled", code(RefusalCode::Throttled)),
            ("RefusalCode::NotFound", code(RefusalCode::NotFound)),
            ("RefusalCode::Timeout", code(RefusalCode::Timeout)),
            (
                "ANNOUNCE_MAGIC",
                quoted(std::str::from_utf8(&announce::ANNOUNCE_MAGIC).unwrap()),
            ),
            (
                "MAX_DISPLAY_NAME_BYTES",
                announce::MAX_DISPLAY_NAME_BYTES.to_string(),
            ),
            (
                "REANNOUNCE_FLOOR_SECS",
                announce::REANNOUNCE_FLOOR_SECS.to_string(),
            ),
            ("HEARTBEAT_SECS", announce::HEARTBEAT_SECS.to_string()),
            (
                "HEARTBEAT_SLACK_SECS",
                announce::HEARTBEAT_SLACK_SECS.to_string(),
            ),
            (
                "HEARTBEAT_PERIOD_SECS",
                announce::HEARTBEAT_PERIOD_SECS.to_string(),
            ),
            (
                "PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT",
                announce::PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT.to_string(),
            ),
            ("PATH_REQUEST_TIMEOUT", secs(announce::PATH_REQUEST_TIMEOUT)),
            ("PEER_TTL", secs(peers::PEER_TTL)),
            ("PEER_STALE_AFTER", secs(peers::PEER_STALE_AFTER)),
            (
                "PEER_TABLE_MAX_ENTRIES",
                peers::PEER_TABLE_MAX_ENTRIES.to_string(),
            ),
            ("KNOCK_TYPE", quoted(knock::KNOCK_TYPE)),
            (
                "KNOCK_INTRO_MAX_CHARS",
                knocks::KNOCK_INTRO_MAX_CHARS.to_string(),
            ),
            ("KNOCK_REQUEST_TIMEOUT", secs(knock::KNOCK_REQUEST_TIMEOUT)),
            ("KNOCK_LINK_TIMEOUT", secs(knock::KNOCK_LINK_TIMEOUT)),
            (
                "KNOCK_GATE_MAX_IDENTITIES",
                knock::KNOCK_GATE_MAX_IDENTITIES.to_string(),
            ),
            ("KNOCK_BUCKET_BURST", knock::KNOCK_BUCKET_BURST.to_string()),
            (
                "KNOCK_BUCKET_REFILL_INTERVAL",
                secs(knock::KNOCK_BUCKET_REFILL_INTERVAL),
            ),
            (
                "KNOCK_DROPS_REMEMBERED",
                knock::KNOCK_DROPS_REMEMBERED.to_string(),
            ),
            (
                "KNOCK_CACHE_MAX_ENTRIES",
                knocks::KNOCK_CACHE_MAX_ENTRIES.to_string(),
            ),
            (
                "KNOCK_CACHE_MAX_PER_IDENTITY",
                knocks::KNOCK_CACHE_MAX_PER_IDENTITY.to_string(),
            ),
            ("STATUS_CARD_VERSION", card::STATUS_CARD_VERSION.to_string()),
            ("STATE_UNKNOWN", card::STATE_UNKNOWN.to_string()),
            ("STATE_IDLE", card::STATE_IDLE.to_string()),
            ("STATE_WORKING", card::STATE_WORKING.to_string()),
            (
                "DISPLAY_NAME_MAX_CHARS",
                card::DISPLAY_NAME_MAX_CHARS.to_string(),
            ),
            ("OBJECTIVE_MAX_CHARS", card::OBJECTIVE_MAX_CHARS.to_string()),
            ("REPO_NAME_MAX_CHARS", card::REPO_NAME_MAX_CHARS.to_string()),
            ("BRANCH_MAX_CHARS", card::BRANCH_MAX_CHARS.to_string()),
            ("TODO_GOAL_MAX_CHARS", card::TODO_GOAL_MAX_CHARS.to_string()),
            ("PEER_MESSAGE_TYPE", quoted(message::PEER_MESSAGE_TYPE)),
            ("PEER_WIRE_VERSION", message::PEER_WIRE_VERSION.to_string()),
            (
                "PEER_TITLE_MAX_CHARS",
                message::PEER_TITLE_MAX_CHARS.to_string(),
            ),
            (
                "PEER_CONTENT_MAX_CHARS",
                message::PEER_CONTENT_MAX_CHARS.to_string(),
            ),
            ("PEER_ID_MAX_CHARS", message::PEER_ID_MAX_CHARS.to_string()),
            (
                "PEER_FIELDS_MAX_BYTES",
                message::PEER_FIELDS_MAX_BYTES.to_string(),
            ),
            (
                "PEER_FIELDS_MAX_DEPTH",
                message::PEER_FIELDS_MAX_DEPTH.to_string(),
            ),
            ("PEER_REQUEST_TIMEOUT", secs(message::PEER_REQUEST_TIMEOUT)),
            ("PEER_LINK_TIMEOUT", secs(message::PEER_LINK_TIMEOUT)),
            ("PENDING_TTL", secs(pending::PENDING_TTL)),
            (
                "PENDING_MAX_ENTRIES",
                pending::PENDING_MAX_ENTRIES.to_string(),
            ),
            ("PEER_WINDOW", secs(limits::PEER_WINDOW)),
            (
                "PEER_RETRY_AFTER_CAPACITY",
                secs(limits::PEER_RETRY_AFTER_CAPACITY),
            ),
            (
                "PEER_LIMITS_MAX_IDENTITIES",
                limits::PEER_LIMITS_MAX_IDENTITIES.to_string(),
            ),
            (
                "DEFAULT_PEER_MAX_CONCURRENT",
                DEFAULT_PEER_MAX_CONCURRENT.to_string(),
            ),
            (
                "DEFAULT_PEER_MAX_MESSAGES_PER_HOUR",
                DEFAULT_PEER_MAX_MESSAGES_PER_HOUR.to_string(),
            ),
            (
                "DEFAULT_PEER_MAX_TOKENS_PER_HOUR",
                DEFAULT_PEER_MAX_TOKENS_PER_HOUR.to_string(),
            ),
            ("ENVOY_RUN_TIMEOUT_SECS", ENVOY_RUN_TIMEOUT_SECS.to_string()),
            (
                "MAX_ACCEPTED_STAMP_COST",
                propagation::MAX_ACCEPTED_STAMP_COST.to_string(),
            ),
            (
                "PROPAGATION_TRANSFER_TIMEOUT",
                secs(propagation::PROPAGATION_TRANSFER_TIMEOUT),
            ),
            (
                "PROPAGATION_REJECT_WINDOW",
                secs(propagation::PROPAGATION_REJECT_WINDOW),
            ),
            (
                "FETCH_REQUEST_TIMEOUT",
                secs(propagation_fetch::FETCH_REQUEST_TIMEOUT),
            ),
            (
                "MAX_LISTED_IDS",
                propagation_fetch::MAX_LISTED_IDS.to_string(),
            ),
            (
                "MAX_WANTS_PER_FETCH",
                propagation_fetch::MAX_WANTS_PER_FETCH.to_string(),
            ),
            (
                "FETCH_TRANSFER_LIMIT_KB",
                propagation_fetch::FETCH_TRANSFER_LIMIT_KB.to_string(),
            ),
            (
                "MAX_FETCHED_MESSAGE_BYTES",
                propagation_fetch::MAX_FETCHED_MESSAGE_BYTES.to_string(),
            ),
            (
                "MIN_FETCHED_MESSAGE_BYTES",
                propagation_fetch::MIN_FETCHED_MESSAGE_BYTES.to_string(),
            ),
            (
                "DEDUP_CAPACITY",
                propagation_fetch::DEDUP_CAPACITY.to_string(),
            ),
            ("DEDUP_HORIZON", secs(propagation_fetch::DEDUP_HORIZON)),
            ("BODY_DEDUP_CAPACITY", node::BODY_DEDUP_CAPACITY.to_string()),
            ("BODY_DEDUP_HORIZON", secs(node::BODY_DEDUP_HORIZON)),
            (
                "MAX_UNKNOWN_SOURCE_DEFERRALS",
                propagation_fetch::MAX_UNKNOWN_SOURCE_DEFERRALS.to_string(),
            ),
            (
                "UNKNOWN_SOURCE_DEFERRAL_HORIZON",
                secs(propagation_fetch::UNKNOWN_SOURCE_DEFERRAL_HORIZON),
            ),
            (
                "MAX_DEFERRED_IDS",
                propagation_fetch::MAX_DEFERRED_IDS.to_string(),
            ),
            (
                "REQUIRED_DELIVERY_STAMP_COST",
                propagation_fetch::REQUIRED_DELIVERY_STAMP_COST.to_string(),
            ),
            (
                "PROPAGATION_NODE_TABLE_MAX_ENTRIES",
                propagation_nodes::PROPAGATION_NODE_TABLE_MAX_ENTRIES.to_string(),
            ),
            (
                "PROPAGATION_NODE_TTL",
                secs(propagation_nodes::PROPAGATION_NODE_TTL),
            ),
            ("FIELD_CUSTOM_TYPE", byte(FIELD_CUSTOM_TYPE)),
            ("FIELD_CUSTOM_DATA", byte(FIELD_CUSTOM_DATA)),
            ("LOGGED_HASH_CHARS", request::LOGGED_HASH_CHARS.to_string()),
            (
                "PEER_INBOX_CAPACITY",
                message::PEER_INBOX_CAPACITY.to_string(),
            ),
            (
                "INBOUND_MAX_ENTRIES",
                pending::INBOUND_MAX_ENTRIES.to_string(),
            ),
            (
                "KNOCK_QUEUE_CAPACITY",
                knock::KNOCK_QUEUE_CAPACITY.to_string(),
            ),
            (
                "ENVOY_QUEUE_MAX",
                crate::config::mesh_envoy::ENVOY_QUEUE_MAX.to_string(),
            ),
            ("TRUST_FILE_VERSION", trust::TRUST_FILE_VERSION.to_string()),
            (
                "KNOCK_RECORD_VERSION",
                knocks::KNOCK_RECORD_VERSION.to_string(),
            ),
            (
                "PENDING_RECORD_VERSION",
                pending::PENDING_RECORD_VERSION.to_string(),
            ),
            (
                "INBOUND_RECORD_VERSION",
                pending::INBOUND_RECORD_VERSION.to_string(),
            ),
            (
                "PREDECESSOR_RECORD_VERSION",
                identity::PREDECESSOR_RECORD_VERSION.to_string(),
            ),
            ("PEER_TABLE_VERSION", peers::PEER_TABLE_VERSION.to_string()),
            (
                "PROPAGATION_STORE_VERSION",
                propagation_fetch::PROPAGATION_STORE_VERSION.to_string(),
            ),
            (
                "PROPAGATION_NODE_TABLE_VERSION",
                propagation_nodes::PROPAGATION_NODE_TABLE_VERSION.to_string(),
            ),
            ("MAX_PARTS", MAX_PARTS.to_string()),
            ("MAX_PARTS_BYTES", MAX_PARTS_BYTES.to_string()),
            ("MAX_INLINE_FILE_TOTAL", MAX_INLINE_FILE_TOTAL.to_string()),
            (
                "DEFAULT_INLINE_MAX_BYTES",
                DEFAULT_INLINE_MAX_BYTES.to_string(),
            ),
            ("LIST_PATH", quoted(request::LIST_PATH)),
            ("FETCH_PATH", quoted(request::FETCH_PATH)),
            (
                "WIRE_PATH_MAX_BYTES",
                wire_path::WIRE_PATH_MAX_BYTES.to_string(),
            ),
            (
                "WIRE_PATH_MAX_SEGMENTS",
                wire_path::WIRE_PATH_MAX_SEGMENTS.to_string(),
            ),
            ("LIST_PAGE_SIZE", shares::LIST_PAGE_SIZE.to_string()),
            ("ALL_LIST_ROWS", shares::ALL_LIST_ROWS.to_string()),
            (
                "DEFAULT_LIST_WALK_BOUND",
                shares::DEFAULT_LIST_WALK_BOUND.to_string(),
            ),
            ("CURSOR_MAX_BYTES", fetch::CURSOR_MAX_BYTES.to_string()),
            ("LIST_PAGE_HEADROOM", fetch::LIST_PAGE_HEADROOM.to_string()),
            (
                "FILE_FETCH_REQUEST_TIMEOUT",
                secs(fetch::FILE_FETCH_REQUEST_TIMEOUT),
            ),
            (
                "DEFAULT_FETCH_MAX_BYTES",
                DEFAULT_FETCH_MAX_BYTES.to_string(),
            ),
            ("MAX_FETCH_FILE_BYTES", MAX_FETCH_FILE_BYTES.to_string()),
            (
                "MAX_FETCH_RESPONSE_BYTES",
                MAX_FETCH_RESPONSE_BYTES.to_string(),
            ),
            ("ABOUT_MAX_CHARS", card::ABOUT_MAX_CHARS.to_string()),
            ("CAPS_MAX_ENTRIES", card::CAPS_MAX_ENTRIES.to_string()),
            ("CAP_MAX_CHARS", card::CAP_MAX_CHARS.to_string()),
            ("ACCESS_PATH", quoted(request::ACCESS_PATH)),
            ("ACCESS_TYPE", quoted(access::ACCESS_TYPE)),
            ("ACCESS_MAX_PATHS", access::ACCESS_MAX_PATHS.to_string()),
            (
                "ACCESS_REASON_MAX_CHARS",
                access::ACCESS_REASON_MAX_CHARS.to_string(),
            ),
            (
                "ACCESS_MAX_PENDING_PER_IDENTITY",
                access::ACCESS_MAX_PENDING_PER_IDENTITY.to_string(),
            ),
            ("DEFAULT_GRANT_TTL", secs(grants::DEFAULT_GRANT_TTL)),
            (
                "SHARES_FILE_VERSION",
                shares::SHARES_FILE_VERSION.to_string(),
            ),
            (
                "GRANT_RECORD_VERSION",
                grants::GRANT_RECORD_VERSION.to_string(),
            ),
            ("DEFAULT_GRANT_USES", grants::DEFAULT_GRANT_USES.to_string()),
            ("GRANT_MAX_PATHS", grants::GRANT_MAX_PATHS.to_string()),
            (
                "FETCH_INLINE_TEXT_MAX_BYTES",
                FETCH_INLINE_TEXT_MAX_BYTES.to_string(),
            ),
            ("MAX_EFFICIENT_SIZE", MAX_EFFICIENT_SIZE.to_string()),
            (
                "PRESENCE_SURFACED_CAP",
                trust::PRESENCE_SURFACED_CAP.to_string(),
            ),
            (
                "ENVOY_SESSION_VERSION",
                envoy_sessions::ENVOY_SESSION_VERSION.to_string(),
            ),
            (
                "ENVOY_SESSION_INDEX_VERSION",
                envoy_sessions::ENVOY_SESSION_INDEX_VERSION.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_SESSIONS",
                DEFAULT_ENVOY_MEMORY_MAX_SESSIONS.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY",
                DEFAULT_ENVOY_MEMORY_MAX_PER_IDENTITY.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_TURNS",
                DEFAULT_ENVOY_MEMORY_MAX_TURNS.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_MAX_BYTES",
                DEFAULT_ENVOY_MEMORY_MAX_BYTES.to_string(),
            ),
            (
                "DEFAULT_ENVOY_MEMORY_TTL_HOURS",
                DEFAULT_ENVOY_MEMORY_TTL_HOURS.to_string(),
            ),
        ]
    }

    #[test]
    fn rust_source_walker_reads_nested_files() {
        let files = rust_files_under(Path::new(env!("CARGO_MANIFEST_DIR")), "src/mesh").unwrap();
        let nested = files
            .iter()
            .find(|(path, _)| path == "src/mesh/request/tests.rs")
            .expect("src/mesh/request/tests.rs is listed");
        assert!(
            nested
                .1
                .contains("fn refusal_codes_round_trip_the_wire_and_reject_other_values")
        );
        assert!(
            !files.iter().any(|(path, _)| path == "src/mesh/pins.rs"),
            "the walk must skip this file so the scanners never read its fixtures"
        );
    }

    #[test]
    fn rust_source_file_lister_keys_each_file_by_its_crate_relative_path() {
        let files = rust_source_files(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mesh = files
            .iter()
            .find(|(path, _)| path == "src/mesh/mod.rs")
            .expect("src/mesh/mod.rs is listed");
        assert!(mesh.1.contains("fn destination_address"));
        assert!(files.iter().all(|(path, _)| path.starts_with("src/")));
        assert!(!files.iter().any(|(path, _)| path == "src/mesh/pins.rs"));
    }

    #[test]
    fn expected_constants_render_to_the_pinned_literal_sequence() {
        let rendered = expected_constants()
            .iter()
            .map(|(_, literal)| literal.as_str())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(rendered, EXPECTED_LITERALS);
    }

    /// The README's mesh section opens with the protocol's name and tagline, then says what
    /// the reference implementation is and what "mesh" names.
    #[test]
    fn readme_mesh_section_leads_with_the_scope_name_tagline_and_reference_sentence() {
        let lines: Vec<&str> = README.lines().collect();
        let headings: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == "### Mesh")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(headings.len(), 1, "### Mesh headings at lines {headings:?}");
        let paragraph = lines[headings[0] + 1..]
            .iter()
            .skip_while(|line| line.trim().is_empty())
            .take_while(|line| !line.trim().is_empty())
            .copied()
            .collect::<Vec<&str>>()
            .join(" ");
        assert!(
            paragraph.starts_with(SCOPE_NAME),
            "the mesh section does not open with {SCOPE_NAME:?}: {paragraph:?}"
        );
        assert!(
            paragraph.contains(SCOPE_TAGLINE),
            "the mesh section lacks the tagline: {paragraph:?}"
        );
        // Assembled at runtime so the mesh source guard does not match this test's text.
        let name = ["Coy", "ote"].concat();
        let reference = format!(
            "{name} is the reference implementation of SCOPE, and \"mesh\" is {name}'s name for its SCOPE feature."
        );
        assert!(
            paragraph.contains(&reference),
            "the mesh section lacks {reference:?}: {paragraph:?}"
        );
    }

    /// The README speaks the SCOPE wire vocabulary only: the pre-SCOPE announce magic and
    /// backtick-wrapped destination prefixes must not appear.
    /// The needles carry their backtick so file names like the log and config do not trip.
    #[test]
    fn readme_never_spells_the_pre_scope_wire_vocabulary() {
        // Assembled at runtime so the mesh source guard does not match this test's text.
        let needles = [
            ["COY", "M"].concat(),
            ["`coy", "ote.mesh"].concat(),
            ["`coy", "ote.peer/"].concat(),
            ["`coy", "ote.knock/"].concat(),
        ];
        let scan = |text: &str| -> Vec<String> {
            text.lines()
                .enumerate()
                .flat_map(|(index, line)| {
                    needles
                        .iter()
                        .filter(move |needle| line.contains(needle.as_str()))
                        .map(move |needle| format!("README.md:{}: spells {needle}", index + 1))
                })
                .collect()
        };
        let fixture = format!("fine line\nthe {} destination\n", needles[1]);
        let control = scan(&fixture);
        assert_eq!(
            control.len(),
            1,
            "the scan does not go red on a fixture: {control:?}"
        );
        let hits = scan(README);
        assert!(hits.is_empty(), "{}", hits.join("\n"));
    }

    /// The `mesh.fetch.max_bytes` row states the setting's range as numbers a user can
    /// compare against their own, and names no second limit under it: the setting alone
    /// bounds a served file.
    #[test]
    fn readme_fetch_row_names_max_bytes_alone() {
        let row = README
            .lines()
            .find(|line| line.starts_with("| `mesh.fetch.max_bytes`"))
            .expect("the README has a mesh.fetch.max_bytes row");
        let clause = format!("`1` to `{MAX_FETCH_FILE_BYTES}` (4 MiB)");
        assert!(
            row.contains(&clause),
            "the fetch row lacks {clause:?}: {row:?}"
        );
        for stale in ["min(max_bytes", "1 MiB", "ceiling", "SINGLE_SEGMENT"] {
            assert!(
                !row.contains(stale),
                "the fetch row still names a limit under the setting ({stale:?}): {row:?}"
            );
        }
    }

    mod envoy_model_row_tests {
        use super::*;

        const CONFIG_EXAMPLE: &str =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.yaml"));
        const CONFIG_TEMPLATE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/config-template.yaml"
        ));
        const AGENT_EXAMPLE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config.agent.example.yaml"
        ));

        /// The `envoy_model:` line and the comment lines that continue it.
        fn envoy_model_block(text: &str) -> Option<String> {
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|line| line.trim_start().starts_with("envoy_model:"))?;
            let continued = lines[at + 1..]
                .iter()
                .take_while(|line| line.trim_start().starts_with('#'));
            Some(
                std::iter::once(&lines[at])
                    .chain(continued)
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }

        /// The envoy model is one node-wide setting: the README row, the example config
        /// and the template say the same thing, and the agent example offers no
        /// per-agent override, since the envoy always runs as the built-in agent.
        #[test]
        fn the_envoy_model_is_one_mesh_setting_described_alike_everywhere() {
            let row = README
                .lines()
                .find(|line| line.starts_with("| `mesh.envoy_model`"))
                .expect("the README has a mesh.envoy_model row");
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            assert_eq!(
                cells[1..4],
                [
                    "`mesh.envoy_model`",
                    "`null`",
                    "Model the envoy answers peers with; `null` uses the configured `model_id` (the envoy never follows a `.model` switch). Behaviour varies by model: if the default refuses or over-escalates, point this at a cheaper model or one from another provider",
                ],
                "{row}"
            );

            let example = envoy_model_block(CONFIG_EXAMPLE).expect("config.example.yaml");
            let template = envoy_model_block(CONFIG_TEMPLATE).expect("config-template.yaml");
            assert_eq!(example, template);
            for clause in [
                "envoy_model: null",
                "Model the envoy answers peers with",
                "null = the configured model_id",
                "the envoy never follows a `.model` switch",
                "if the default refuses or over-escalates",
            ] {
                assert!(example.contains(clause), "{clause:?} not in {example}");
            }
            assert_eq!(envoy_model_block(AGENT_EXAMPLE), None);
        }
    }

    /// The envoy memory file name for one (identity, thread) is fixed by this vector: a
    /// change to the digest input would silently orphan every record on disk.
    #[test]
    fn envoy_session_key_is_the_truncated_sha256_of_identity_nul_thread() {
        let identity = "0123456789abcdef0123456789abcdef";
        let key = envoy_sessions::session_key(identity, "thread-one").unwrap();

        assert_eq!(key, "8ebad0226f3717c3cebe929e4b7c49ca");
        assert_eq!(key.len(), 32);
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{key}"
        );
        assert_ne!(
            key,
            envoy_sessions::session_key(identity, "thread-two").unwrap()
        );
        assert_ne!(
            key,
            envoy_sessions::session_key("fedcba9876543210fedcba9876543210", "thread-one").unwrap()
        );
    }

    fn follows(line: &str, needle: &str, accept: impl Fn(&[u8]) -> bool) -> bool {
        line.match_indices(needle)
            .any(|(at, _)| accept(&line.as_bytes()[at + needle.len()..]))
    }

    /// `follows`, with the needle starting a word, so a `render_spec(s)` call is no citation.
    fn follows_word(line: &str, needle: &str, accept: impl Fn(&[u8]) -> bool) -> bool {
        let bytes = line.as_bytes();
        line.match_indices(needle).any(|(at, _)| {
            is_word_boundary(at.checked_sub(1).and_then(|before| bytes.get(before)))
                && accept(&bytes[at + needle.len()..])
        })
    }

    /// A line that cites a planning artefact (a task id, a lettered review criterion, a
    /// review round, a ruling, a commit, a letter-dash-number plan label in a comment)
    /// instead of the behaviour it tests, in any letter case. The needles are assembled at
    /// runtime so this file does not spell them. A label quoted inside a code span or a
    /// string literal is fixture input, not a citation (the lint tests feed such strings to
    /// their scanners on purpose), and a line that names the `ILLUSTRATIVE_IDS` list is the
    /// fixture that keeps one such id on purpose.
    fn cites_a_plan_label(line: &str) -> bool {
        if line.contains("ILLUSTRATIVE_IDS") {
            return false;
        }
        let line = line.to_ascii_lowercase();
        let task = ["task", "-"].concat();
        let lettered = [
            "criterion".to_string(),
            "acceptance".to_string(),
            "spec".to_string(),
            ["usage", " probe"].concat(),
            "amendment".to_string(),
        ];
        let lettered_numbered = [
            ["b", "-"].concat(),
            ["g", "-"].concat(),
            ["t", "-"].concat(),
        ];
        let rounds = [
            ["probe,", " round "].concat(),
            ["review", " round "].concat(),
            ["criteria", " "].concat(),
        ];
        let ruling = ["ruling", " "].concat();
        let shorthand = "(r";
        let round_dash = ["round", "-"].concat();
        let plain = [
            ["plan", " criterion"].concat(),
            ["plan", " ruling"].concat(),
            ["user", " ruling"].concat(),
        ];
        let commit = [" fix", " commit"];
        let letter_in_parens = |rest: &[u8]| {
            rest.first() == Some(&b'(')
                && rest.get(1).is_some_and(u8::is_ascii_lowercase)
                && rest.get(2) == Some(&b')')
        };
        let after_optional_round = |rest: &[u8]| {
            let rest = match rest {
                [b' ', b'r', digits @ ..] if digits.first().is_some_and(u8::is_ascii_digit) => {
                    &digits[digits.iter().take_while(|b| b.is_ascii_digit()).count()..]
                }
                rest => rest,
            };
            letter_in_parens(rest.strip_prefix(b" ").unwrap_or(rest))
        };
        let after_a_short_hash = |before: &[u8]| {
            let hex = before
                .iter()
                .rev()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            let token = &before[before.len() - hex..];
            (7..=40).contains(&hex)
                && token.iter().any(u8::is_ascii_digit)
                && before[..before.len() - hex]
                    .last()
                    .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
        };
        let round_shorthand = |rest: &[u8]| {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            let rest = &rest[digits..];
            digits > 0
                && (rest.first() == Some(&b')')
                    || rest.strip_prefix(b" ").is_some_and(letter_in_parens))
        };
        let ruling_shorthand_at_start = |text: &str| {
            let bytes = text.as_bytes();
            bytes.first() == Some(&b'r') && bytes.get(1).is_some_and(u8::is_ascii_digit) && {
                let digits = bytes[1..].iter().take_while(|b| b.is_ascii_digit()).count();
                bytes.get(1 + digits) == Some(&b':')
            }
        };
        let comment_text = line
            .trim_start()
            .strip_prefix("//")
            .map(|text| text.trim_start_matches('/').trim_start());
        let starts_with_digit = |rest: &[u8]| rest.first().is_some_and(u8::is_ascii_digit);
        let unquoted = without_quoted(&line);
        follows(&unquoted, &task, starts_with_digit)
            || lettered
                .iter()
                .any(|needle| follows_word(&unquoted, needle, after_optional_round))
            || comment_text.is_some_and(|text| {
                lettered_numbered
                    .iter()
                    .any(|needle| follows_word(&without_quoted(text), needle, starts_with_digit))
            })
            || comment_text.is_some_and(|text| letter_in_parens(text.as_bytes()))
            || rounds
                .iter()
                .chain(std::iter::once(&ruling))
                .any(|needle| follows(&unquoted, needle, starts_with_digit))
            || plain
                .iter()
                .any(|needle| unquoted.contains(needle.as_str()))
            || comment_text.is_some_and(|text| follows(text, shorthand, round_shorthand))
            || comment_text.is_some_and(ruling_shorthand_at_start)
            || follows(&unquoted, &round_dash, starts_with_digit)
            || commit.iter().any(|needle| {
                unquoted
                    .match_indices(needle)
                    .any(|(at, _)| after_a_short_hash(&unquoted.as_bytes()[..at]))
            })
    }

    /// `line` without its code spans and double-quoted segments. A quote escaped with a
    /// backslash does not close the segment, so a string that quotes another string is one
    /// segment.
    fn without_quoted(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut rest = strip_spans(line);
        while let Some(open) = rest.find('"') {
            let bytes = rest.as_bytes();
            let mut at = open + 1;
            while at < bytes.len() && bytes[at] != b'"' {
                at += if bytes[at] == b'\\' { 2 } else { 1 };
            }
            if at >= bytes.len() {
                break;
            }
            out.push_str(&rest[..open]);
            out.push(' ');
            rest = rest[at + 1..].to_string();
        }
        out.push_str(&rest);
        out
    }

    #[test]
    fn plan_label_scanner_trips_each_shape_and_spares_prose() {
        for hit in [
            ["// see TASK", "-118 for why"].concat(),
            ["// see task", "-118 for why"].concat(),
            ["/// Usage probe, criterion", " (b): the root"].concat(),
            ["/// Criterion", " (b): the root"].concat(),
            ["/// acceptance", " (c): the peer"].concat(),
            ["// the spec", "(a) says"].concat(),
            ["/// usage", " probe r2 (d): bare"].concat(),
            ["/// amendment", " (b) covers"].concat(),
            "/// (x) the second reply".to_string(),
            "// (a) the second reply".to_string(),
            ["// ruling", " 3 settled it"].concat(),
            ["// per the user", " ruling"].concat(),
            ["// ---- review", " round 2 ----"].concat(),
            ["// ---- usage probe,", " round 2 ----"].concat(),
            ["/// Plan", " criterion (f): at every cap"].concat(),
            ["// the plan", " ruling was"].concat(),
            ["/// usage", " probe r10 (b): the tenth"].concat(),
            ["// settled (R", "3): the reply"].concat(),
            ["// ---- usage probe (r", "4) ----"].concat(),
            ["// settled (r", "2 (b)): the reply"].concat(),
            ["// criteria", " 3+4 hold"].concat(),
            ["// the 3a3d1d1", " fix narrowed it"].concat(),
            [
                "// after 0123456789abcdef0123456789abcdef01234567",
                " commit",
            ]
            .concat(),
            ["// the preview prints first (B", "-40)"].concat(),
            ["/// g", "-12 covers the relay"].concat(),
            ["// per t", "-3 the cap holds"].concat(),
            ["// see B", "-7"].concat(),
            ["// R", "8: judged before anything is printed"].concat(),
            ["// r", "3: the peer refuses the reference"].concat(),
            ["// The round", "-3 README claimed"].concat(),
        ] {
            assert!(cites_a_plan_label(&hit), "{hit:?}");
        }
        for miss in [
            "// the TASK list is drained".to_string(),
            "// a criterion (the first) applies".to_string(),
            "// the rule (a) above".to_string(),
            "// a spec (RFC) is cited".to_string(),
            "// reviewed in round 2 of the audit".to_string(),
            "// the ruling stands".to_string(),
            "// a planned criterion is unmet".to_string(),
            "// B6 and G8 are hex here".to_string(),
            "// the R7 relay frames it".to_string(),
            "// the R7 relay (r7 for short) frames it".to_string(),
            "// r7 frames it".to_string(),
            "// round trip".to_string(),
            "// the relay (r7, lowercase on the wire) frames it".to_string(),
            "// Err(RequestError::Shutdown) ends it".to_string(),
            "assert_eq!(r1.len(), 2);".to_string(),
            "// 9f1c3b0e6d8a4f2b9c7e1a5d3b8f6c04 is a wire id".to_string(),
            "// a fix for the relay".to_string(),
            "// the cache fix narrowed it".to_string(),
            "// a defaced fix is reverted".to_string(),
            "// the criteria differ".to_string(),
            "// render_spec(s) builds the table".to_string(),
            ["// the rule quotes `// TASK", "-002` as its example"].concat(),
            ["let id = \"TASK", "-002\";"].concat(),
            ["const ILLUSTRATIVE_IDS: [&str; 1] = [\"TASK", "-002\"];"].concat(),
            "// the a1b2c3 hash names it".to_string(),
            "// sub-1 is the first child".to_string(),
            "// the b-tree is balanced".to_string(),
            "// t-shirt sizing".to_string(),
            ["let label = \"B", "-40\";"].concat(),
            ["  models: [{name: g", "-1}]"].concat(),
            ["/// Node A asks in thread `t", "-1`; node B answers"].concat(),
            ["\"// per ruling", " 3 this stays\","].concat(),
            ["\"Review Round", " 2 found it.\".to_string(),"].concat(),
            ["\"let title = \\\"TASK", "-99 in a string\\\";\","].concat(),
            ["code.push(\"// the plan", " ruling kept it\".to_string());"].concat(),
            ["\"/// criterion", " (b) quoted\","].concat(),
            ["\"the 3a3d1d1", " fix commit\","].concat(),
        ] {
            assert!(!cites_a_plan_label(&miss), "{miss:?}");
        }
    }

    #[test]
    fn source_comments_cite_behaviour_not_plan_labels() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<(String, String)> = rust_source_files(root).unwrap();
        files.extend(rust_files_under(root, "tests").unwrap());
        let hits: Vec<String> = files
            .iter()
            .flat_map(|(path, source)| {
                source
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| cites_a_plan_label(line))
                    .map(move |(index, line)| format!("{path}:{}: {}", index + 1, line.trim()))
            })
            .collect();
        assert!(
            hits.is_empty(),
            "code comments describe behaviour; say the behaviour instead of a plan label:\n{}",
            hits.join("\n")
        );
    }

    /// `source` line for line, so line numbers hold, with every `#[cfg(test)]`-attributed
    /// inline module and every plain `//` comment line blanked. Doc comments (`///`,
    /// `//!`) stay: they render as rustdoc and as `clap` help. A top-level module closes
    /// at the first `}` on a line of its own, as rustfmt lays it out.
    fn lines_outside_test_modules(source: &str) -> Vec<&str> {
        let mut lines: Vec<&str> = source.lines().collect();
        let mut at = 0;
        while at < lines.len() {
            let opens_test_module = lines[at] == "#[cfg(test)]"
                && lines.get(at + 1).is_some_and(|opener| {
                    opener.ends_with('{')
                        && (opener.starts_with("mod ") || opener.starts_with("pub(crate) mod "))
                });
            if opens_test_module {
                while at < lines.len() && lines[at] != "}" {
                    lines[at] = "";
                    at += 1;
                }
            } else if is_a_plain_comment(lines[at]) {
                lines[at] = "";
            }
            at += 1;
        }
        lines
    }

    fn is_a_plain_comment(line: &str) -> bool {
        let rest = line.trim_start().strip_prefix("//");
        rest.is_some_and(|rest| !rest.starts_with('/') && !rest.starts_with('!'))
    }

    mod mesh_text_sinks {
        use super::*;
        use std::collections::BTreeMap;

        /// The functions that put mesh text on the terminal or render it for one, by file:
        /// the free sinks in `src/repl/mesh.rs`, the prompt printer, the stderr fallback
        /// of `MeshSlot::notify`, the span renderer every line those two print goes
        /// through, and the REPL's two error printers. Each must call a renderer unless it
        /// is an unrendered sink.
        const SINKS: [(&str, &str); 9] = [
            ("src/repl/mesh.rs", "out_text"),
            ("src/repl/mesh.rs", "err_text"),
            ("src/repl/mesh.rs", "out_path_text"),
            ("src/repl/mesh.rs", "err_path_text"),
            ("src/repl/printer.rs", "notify"),
            ("src/mesh/node.rs", "notify"),
            ("src/mesh/notify.rs", "render_span"),
            ("src/render/mod.rs", "render_error"),
            ("src/render/mod.rs", "render_command_error"),
        ];

        /// Sinks that print byte for byte, each with the reason it must not render hints.
        const UNRENDERED_SINKS: [(&str, &str, &str); 3] = [
            (
                "src/repl/mesh.rs",
                "out_path_text",
                "prints paths and share patterns, and `.vault` would become `/vault`",
            ),
            (
                "src/repl/mesh.rs",
                "err_path_text",
                "the stderr twin of out_path_text, for refusals that quote patterns",
            ),
            (
                "src/render/mod.rs",
                "render_error",
                "prints errors raised outside the mesh verb, which carry no mesh hint",
            ),
        ];

        const RENDERERS: [&str; 3] = [
            "render_hints",
            "render_notification_lines",
            "command_error_text",
        ];

        const PRINT_MACROS: [&str; 5] = [
            "println!",
            "eprintln!",
            "print!",
            "eprint!",
            "render_error(",
        ];

        fn at_word_boundary<'a>(
            code: &'a str,
            needle: &'a str,
        ) -> impl Iterator<Item = usize> + 'a {
            code.match_indices(needle)
                .filter(|(at, _)| is_word_boundary(code.as_bytes()[..*at].last()))
                .map(|(at, _)| at)
        }

        /// A print macro, `render_error`, or a `write!`/`writeln!` whose target is a
        /// stdout or stderr handle; or a stdout or stderr handle acquired or imported,
        /// since an alias of one prints past every check above. Asking a handle whether
        /// it is a terminal prints nothing.
        fn prints_directly(code: &str) -> bool {
            PRINT_MACROS
                .iter()
                .any(|needle| at_word_boundary(code, needle).next().is_some())
                || ["write!(", "writeln!("].iter().any(|needle| {
                    at_word_boundary(code, needle).any(|at| {
                        let target = code[at + needle.len()..].split(',').next().unwrap_or("");
                        target.contains("stdout") || target.contains("stderr")
                    })
                })
                || ["stdout", "stderr"].iter().any(|handle| {
                    at_word_boundary(code, handle).any(|at| {
                        let rest = &code[at + handle.len()..];
                        let named =
                            !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
                        let acquired =
                            rest.starts_with("()") && !rest.starts_with("().is_terminal()");
                        acquired || (named && code.starts_with("use "))
                    })
                })
        }

        /// The name of the function `code` opens, when it opens one with a body.
        fn fn_name(code: &str) -> Option<&str> {
            if code.ends_with(';') {
                return None;
            }
            let rest = code
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub(super) ")
                .trim_start_matches("pub ")
                .trim_start_matches("async ")
                .strip_prefix("fn ")?;
            rest.split(['(', '<']).next()
        }

        /// Every direct print outside a sink, and every sink without a renderer call that
        /// is not an unrendered sink; plus the sinks `source` defines. A sink is a free
        /// function or a method one level into an `impl`.
        fn scan(path: &str, source: &str) -> (Vec<String>, Vec<String>) {
            let mut hits = Vec::new();
            let mut rendered: BTreeMap<&str, bool> = BTreeMap::new();
            let mut item: Option<(&str, usize)> = None;
            let mut opened_write = String::new();
            for (index, line) in lines_outside_test_modules(source).into_iter().enumerate() {
                let code = line.trim_start();
                let indent = line.len() - code.len();
                if code.starts_with("//") {
                    continue;
                }
                if let Some((_, open)) = item
                    && indent == open
                    && code.starts_with('}')
                {
                    item = None;
                    continue;
                }
                if item.is_none() && (indent == 0 || indent == 4) {
                    let sink = fn_name(code)
                        .filter(|name| SINKS.contains(&(path, *name)))
                        .map(|name| (name, indent));
                    if let Some((name, _)) = sink {
                        rendered.entry(name).or_insert(false);
                        item = sink;
                        continue;
                    }
                }
                let uncommented = code.split("//").next().unwrap_or(code);
                // rustfmt puts a long `writeln!(` target on the next line, so an opened
                // call is read together with the line after it.
                let joined = format!("{opened_write}{code}");
                opened_write.clear();
                if uncommented.trim_end().ends_with("write!(")
                    || uncommented.trim_end().ends_with("writeln!(")
                {
                    opened_write = uncommented.trim_end().to_string();
                    continue;
                }
                match item {
                    Some((name, _)) if RENDERERS.iter().any(|r| uncommented.contains(r)) => {
                        rendered.insert(name, true);
                    }
                    Some(_) => {}
                    None if prints_directly(&joined) => {
                        hits.push(format!("{path}:{}: {code}", index + 1));
                    }
                    None => {}
                }
            }
            for (name, rendered) in &rendered {
                let unrendered = UNRENDERED_SINKS
                    .iter()
                    .any(|(file, sink, _)| *file == path && sink == name);
                if !rendered && !unrendered {
                    hits.push(format!("{path}: `{name}` prints without a renderer"));
                }
            }
            (hits, rendered.into_keys().map(str::to_string).collect())
        }

        #[test]
        fn sink_scanner_trips_each_shape_and_spares_the_sinks() {
            const REPL: &str = "src/repl/mesh.rs";
            const NODE: &str = "src/mesh/node.rs";
            for (path, red, expected) in [
                (REPL, "fn show() {\n    println!(\"x\");\n}\n", 1),
                (REPL, "fn show() {\n    eprint!(\"x\");\n}\n", 1),
                (REPL, "fn show() {\n    let _ = 1; print!(\"x\");\n}\n", 1),
                (
                    REPL,
                    "fn show() {\n    let _ = writeln!(stderr, \"x\");\n}\n",
                    1,
                ),
                (
                    REPL,
                    "fn show() {\n    write!(std::io::stdout().lock(), \"x\").ok();\n}\n",
                    1,
                ),
                (
                    REPL,
                    "fn show() {\n    let _ = writeln!(\n        stderr,\n        \"x\"\n    );\n}\n",
                    1,
                ),
                (
                    REPL,
                    "fn show() {\n    let e = std::io::stderr();\n    let _ = e;\n}\n",
                    1,
                ),
                (
                    REPL,
                    "use std::io::stderr as quiet;\nfn show() {\n    let _ = quiet;\n}\n",
                    1,
                ),
                (REPL, "use std::io::{Write, stdout};\nfn show() {}\n", 1),
                (
                    REPL,
                    "fn show(err: Error) {\n    render_error(err);\n}\n",
                    1,
                ),
                (
                    REPL,
                    "fn show(err: Error) {\n    crate::render::render_error(err);\n}\n",
                    1,
                ),
                (
                    REPL,
                    "pub(crate) fn err_text(text: &str) {\n    eprintln!(\"{text}\");\n}\n",
                    1,
                ),
                (
                    REPL,
                    "pub(super) fn out_text(text: &str) {\n    println!(\"{text}\");\n}\n",
                    1,
                ),
                (
                    REPL,
                    "fn out_text(text: &str) {\n    // render_hints is not called here\n    println!(\"{text}\");\n}\n",
                    1,
                ),
                (
                    REPL,
                    "fn out_text(text: &str) {\n    println!(\"{text}\"); // render_hints\n}\n",
                    1,
                ),
                (
                    REPL,
                    "fn out_text(text: &str) {\n    println!(\"{text}\");\n}\nfn after() {\n    println!(\"y\");\n}\n",
                    2,
                ),
                (
                    NODE,
                    "impl MeshSlot {\n    pub(crate) fn notify(&self, rendered: RenderedNotification) {\n        for line in rendered.lines() {\n            let _ = writeln!(stderr, \"{line}\");\n        }\n    }\n}\n",
                    1,
                ),
                (
                    "src/fixture.rs",
                    "impl Node {\n    fn out_text(&self) {\n        println!(\"x\");\n    }\n}\n",
                    1,
                ),
            ] {
                let (hits, _) = scan(path, red);
                assert_eq!(hits.len(), expected, "{red:?}: {hits:?}");
            }
            for (path, green) in [
                (
                    REPL,
                    "pub(crate) fn out_text(text: &str) {\n    println!(\"{}\", live_prefix().render_hints(text));\n}\n",
                ),
                (
                    REPL,
                    "pub(super) fn err_text(text: &str) {\n    eprintln!(\"{}\", live_prefix().render_hints(text));\n}\n",
                ),
                (
                    REPL,
                    "pub(crate) fn out_path_text(text: &str) {\n    println!(\"{text}\");\n}\n",
                ),
                (
                    REPL,
                    "pub(crate) fn err_path_text(text: &str) {\n    eprintln!(\"{text}\");\n}\n",
                ),
                (
                    NODE,
                    "impl MeshSlot {\n    pub(crate) fn notify(&self, rendered: RenderedNotification) {\n        for line in rendered.render_notification_lines(prefix) {\n            let _ = writeln!(stderr, \"{line}\");\n        }\n    }\n}\n",
                ),
                (REPL, "/// `println!` is not called here\nfn quiet() {}\n"),
                (
                    REPL,
                    "fn coloured() -> bool {\n    std::io::stderr().is_terminal()\n}\n",
                ),
                (
                    REPL,
                    "use crate::utils::stderr_error_text;\nfn quiet() {}\n",
                ),
                (
                    "src/render/mod.rs",
                    "pub fn render_error(err: anyhow::Error) {\n    eprintln!(\"{}\", pretty_error(&err));\n}\n",
                ),
                (
                    REPL,
                    "fn quiet() {\n    // println! would skip the sink\n}\n",
                ),
                (
                    REPL,
                    "#[cfg(test)]\nmod tests {\n    fn noisy() {\n        println!(\"x\");\n    }\n}\n",
                ),
                (
                    REPL,
                    "fn quiet(out: &mut String) {\n    let _ = writeln!(out, \"x\");\n}\n",
                ),
                (
                    REPL,
                    "fn shown(err: Error, prefix: CommandPrefix) {\n    render_command_error(err, prefix);\n}\n",
                ),
            ] {
                let (hits, _) = scan(path, green);
                assert!(hits.is_empty(), "{green:?}: {hits:?}");
            }
        }

        /// Mesh text reaches the terminal only through the sinks, so every line follows
        /// the configured command prefix and paths are printed as they are. The
        /// conformance and fuzz harnesses print reports, not mesh text.
        #[test]
        fn mesh_text_is_printed_only_through_the_sinks() {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"));
            let mut files = rust_files_under(root, "src/mesh").unwrap();
            files.retain(|(path, _)| {
                !path.starts_with("src/mesh/conformance/") && !path.starts_with("src/mesh/fuzz/")
            });
            for extra in [
                "src/repl/mesh.rs",
                "src/repl/printer.rs",
                "src/render/mod.rs",
                "src/repl/idle.rs",
                "src/config/mesh_envoy.rs",
            ] {
                files.push((
                    extra.to_string(),
                    std::fs::read_to_string(root.join(extra)).unwrap(),
                ));
            }
            let mut hits = Vec::new();
            let mut found: Vec<(String, String)> = Vec::new();
            for (path, source) in &files {
                let (file_hits, sinks) = scan(path, source);
                hits.extend(file_hits);
                found.extend(sinks.into_iter().map(|sink| (path.clone(), sink)));
            }
            found.sort();
            let mut expected: Vec<(String, String)> = SINKS
                .iter()
                .map(|(path, name)| (path.to_string(), name.to_string()))
                .collect();
            expected.sort();
            assert_eq!(
                found, expected,
                "every listed sink must exist where it is listed"
            );
            assert!(
                hits.is_empty(),
                "print mesh text through out_text or err_text, or out_path_text/err_path_text for a path:\n{}",
                hits.join("\n")
            );
        }
    }

    /// The workspace config directory's name reaches the code through
    /// `paths::workspace_config_dir_name` and `paths::workspace_config_dirs`, so an
    /// override renames it everywhere at once, help and display strings included. Only
    /// `paths.rs` and the constant's own definition may spell it; test code may, since a
    /// fixture lays out a directory by its default name. A file whose module is declared
    /// under `#[cfg(test)]` is test code wholesale. A plain `//` comment may say what it
    /// likes; a doc comment is held to the helper because it renders, as rustdoc or as
    /// `clap` help. The name counts when it stands as a word: opened by a quote, a
    /// backtick, a space, a slash, a brace or a parenthesis and not run on into a longer
    /// identifier, so `.coyote_password` and `.coyote-case-probe` are other names.
    #[test]
    fn only_the_paths_helper_spells_the_workspace_config_dir_name() {
        // Assembled at runtime so the scan does not match this test's own text.
        let constant = ["WORKSPACE_", "COYOTE_DIR_NAME"].concat();
        let name = [".coy", "ote"].concat();
        let spells_the_name = |line: &str| {
            line.match_indices(&name).any(|(at, _)| {
                // The name is a whole word: whatever precedes it is not part of an
                // identifier, and neither is whatever follows, so `.coyote_password` and
                // `.coyote-case-probe` are other names while `\.coyote\` and `=.coyote` are
                // hits. A string escape such as `\n` or `\t` ends in a letter but is not a
                // word either.
                let part_of_a_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
                let before = &line[..at];
                let after_an_escape = before.ends_with("\\n") || before.ends_with("\\t");
                (after_an_escape || !before.chars().next_back().is_some_and(part_of_a_word))
                    && !line[at + name.len()..]
                        .chars()
                        .next()
                        .is_some_and(part_of_a_word)
            })
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = rust_source_files(root).unwrap();
        let test_modules: HashSet<String> = files
            .iter()
            .flat_map(|(_, source)| {
                source
                    .lines()
                    .zip(source.lines().skip(1))
                    .filter_map(|(attribute, item)| {
                        (attribute.trim() == "#[cfg(test)]")
                            .then(|| item.trim().trim_start_matches("pub(crate) "))
                            .and_then(|item| item.strip_prefix("mod "))
                            .and_then(|item| item.strip_suffix(';'))
                            .map(str::to_string)
                    })
            })
            .collect();
        let is_test_file = |path: &str| {
            path.split('/')
                .any(|part| test_modules.contains(part.strip_suffix(".rs").unwrap_or(part)))
        };
        let scan = |path: &str, source: &str| -> Vec<String> {
            if path == "src/config/paths.rs" || is_test_file(path) {
                return Vec::new();
            }
            lines_outside_test_modules(source)
                .iter()
                .enumerate()
                .filter(|(_, line)| {
                    let defines_the_constant = path == "src/config/mod.rs"
                        && line
                            .trim_start()
                            .starts_with(&format!("pub(crate) const {constant}"));
                    !defines_the_constant && (line.contains(&constant) || spells_the_name(line))
                })
                .map(|(index, line)| format!("{path}:{}: {}", index + 1, line.trim()))
                .collect()
        };
        for red in [
            format!(
                "fn help() -> &'static str {{\n    \"Save under {name}/ in the workspace\"\n}}\n"
            ),
            format!("/// Disable loading workspace macros from {name}/macros\n"),
            format!("let dir = \"{name}\";\n"),
            format!("let file = root.join(\"/{name}/mcp.json\");\n"),
            format!("/// The `{name}` directory holds it.\n"),
            format!("/// Saved under {name}, beside the sources.\n"),
            format!("let name = {constant};\n"),
            format!("let key = \"C:\\\\Users\\\\me\\\\{name}\\\\mesh\\\\identity.key\";\n"),
            format!("let entry = \"target/\\n{name}/memory/\\n\";\n"),
            format!("/// e.g. COYOTE_WORKSPACE_CONFIG_DIR={name}\n"),
        ] {
            let control = scan("src/fixture.rs", &red);
            assert_eq!(
                control.len(),
                1,
                "the scan does not go red on {red:?}: {control:?}"
            );
        }
        for green in [
            format!("let probe = \"{name}-case-probe\";\n"),
            format!("let file = home.join(\"{name}_password\");\n"),
            format!("// a {name} comment is not help\n"),
        ] {
            let control = scan("src/fixture.rs", &green);
            assert!(
                control.is_empty(),
                "the scan goes red on {green:?}: {control:?}"
            );
        }
        let hits: Vec<String> = files
            .iter()
            .flat_map(|(path, source)| scan(path, source))
            .collect();
        assert!(
            hits.is_empty(),
            "spell the workspace config directory through paths::workspace_config_dir_name \
             or paths::workspace_config_dirs:\n{}",
            hits.join("\n")
        );
    }

    /// The fetch response bound is the largest file plus framing, and a reply carrying
    /// exactly the largest file fits under it.
    #[test]
    fn fetch_response_bound_is_the_file_bound_plus_framing() {
        assert_eq!(
            MAX_FETCH_RESPONSE_BYTES,
            MAX_FETCH_FILE_BYTES as usize + 4096
        );
        let largest = ResponseFrame::new(
            RequestId::from([0; 16]),
            Value::Map(vec![
                (Value::from("v"), Value::from(1)),
                (Value::from("status"), Value::from("ok")),
                (Value::from("size"), Value::from(MAX_FETCH_FILE_BYTES)),
                (Value::from("sha256"), Value::Binary(vec![0; 32])),
                (
                    Value::from("bytes"),
                    Value::Binary(vec![0; MAX_FETCH_FILE_BYTES as usize]),
                ),
            ]),
        )
        .encode();
        assert!(
            largest.len() <= MAX_FETCH_RESPONSE_BYTES,
            "{} bytes, max {MAX_FETCH_RESPONSE_BYTES}",
            largest.len()
        );
    }

    /// A response that leaves as a resource is compressed at every place it is sent from:
    /// the paths the node registers carry `auto_compress` for the replies the library
    /// sends, and the test listener that stands in for the node registers its paths the
    /// same way. The adapter sends nothing itself: a settled reply is handed back for the
    /// library to send under that same flag, so there is no second send site to keep in
    /// step.
    #[test]
    fn every_response_resource_is_sent_with_compression_on() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = rust_source_files(root).unwrap();
        // Whitespace and the commas before a closing parenthesis go, so the needles hold
        // whichever way rustfmt lays an argument list out.
        let flattened = |name: &str| -> String {
            let (_, source) = files
                .iter()
                .find(|(path, _)| path == name)
                .unwrap_or_else(|| panic!("{name} is not under src"));
            source
                .split_whitespace()
                .collect::<String>()
                .replace(",)", ")")
        };
        for (name, adapter) in [
            ("src/mesh/node.rs", "request_handler"),
            ("src/mesh/mod.rs", "adapter"),
        ] {
            let source = flattened(name);
            let registration =
                format!(".register_request_handler(path,{adapter}.clone(),RequestAllow::All,true)");
            assert_eq!(
                source.matches(".register_request_handler(").count(),
                1,
                "{name} registers request paths at one site"
            );
            assert!(
                source.contains(&registration),
                "{registration} is not in {name}"
            );
        }
        let handler = flattened("src/mesh/request/handler.rs");
        assert!(
            !handler.contains("Transport") && !handler.contains(".send_"),
            "handler.rs reaches the transport or sends something itself; the adapter can only hand a value back for the library to send"
        );
    }
}
