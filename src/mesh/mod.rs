#![deny(unsafe_code)]

pub(crate) mod access;
mod announce;
pub(crate) mod brief;
pub(crate) mod card;
pub(crate) mod card_cadence;
pub(crate) mod card_refusals;
#[cfg(test)]
pub(crate) mod conformance;
pub(crate) mod envoy;
pub(crate) mod envoy_sessions;
pub(crate) mod events;
pub(crate) mod fetch;
#[cfg(test)]
mod fuzz;
pub(crate) mod grants;
pub(crate) mod identity;
pub(crate) mod idle;
pub(crate) mod inbox;
mod instances;
pub(crate) mod knock;
pub(crate) mod knocks;
mod lan_count;
pub(crate) mod limits;
mod lock;
pub(crate) mod message;
mod node;
pub(crate) mod notify;
mod peers;
pub(crate) mod pending;
pub(crate) mod people;
#[cfg(test)]
mod pins;
mod propagation;
mod propagation_fetch;
mod propagation_nodes;
mod protocol;
pub(crate) mod request;
mod routes;
// pub(crate): the REPL pins the shared refusal wording in its tests.
pub(crate) mod schema;
mod sent_threads;
pub(crate) mod shares;
pub(crate) mod snapshot;
pub(crate) mod trust;
mod upstream;
#[cfg(test)]
mod wire_golden;
pub(crate) mod wire_path;

pub(crate) use announce::{HEARTBEAT_PERIOD_SECS, HEARTBEAT_SECS, PATH_REQUEST_TIMEOUT};
pub(crate) use instances::InstanceSession;
pub(crate) use lan_count::{ReadEffort, known_host_total};
#[cfg(test)]
pub(crate) use node::MESH_PROGRESS_MIN_INTERVAL;
#[cfg(test)]
pub(crate) use node::dismiss_held_by_envoy;
#[cfg(test)]
pub(crate) use node::session_destination_name;
pub(crate) use node::{
    AddressName, AlreadyClosed, DialAdvice, DialFailure, HUMAN_DECLINED, InterfaceState,
    InterfaceStatus, LocalEndpoint, MESH_ALREADY_ON, MeshPaths, MeshRuntime, MeshSlot, NameTarget,
    NodeOptions, already_closed, loopback_endpoint, peer_session_name, refusal_reply,
};
pub(crate) use peers::{PEER_STALE_AFTER, PEER_TTL, PeerRecord};
pub(crate) use propagation_fetch::{
    FetchError, FetchReport, LoggingInboundSink, MAX_WANTS_PER_FETCH,
};
pub(crate) use propagation_nodes::{
    CONFIGURED_NODE_ASK_GAP, NodePurpose, PROPAGATION_NODE_TTL, PropagationNodeRecord,
};
pub(crate) use request::{RequestOptions, redact_hashes, short};
pub(crate) use routes::with_progress;

use crate::config::sanitize_display_text;
use anyhow::{Context, Result, bail};
use rns_transport::hash::{AddressHash, Hash};
use sha2::Digest;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Where mesh state that is safe to lose lives.
pub(crate) fn mesh_cache_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("mesh")
}

/// Where mesh state the user curates lives.
pub(crate) fn mesh_config_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("mesh")
}

/// RFC 3339 UTC to the second, the timestamp form every human-readable mesh file uses.
/// A time past either end of the calendar renders as that end rather than failing,
/// since some of the times shown were a peer's to choose.
pub(crate) fn rfc3339_utc(time: SystemTime) -> String {
    let (whole_seconds, clamp) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => (
            i64::try_from(since.as_secs()).ok(),
            chrono::DateTime::<chrono::Utc>::MAX_UTC,
        ),
        Err(before) => {
            let before = before.duration();
            let secs = before.as_secs() + u64::from(before.subsec_nanos() > 0);
            (
                i64::try_from(secs).ok().map(|secs| -secs),
                chrono::DateTime::<chrono::Utc>::MIN_UTC,
            )
        }
    };
    whole_seconds
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .unwrap_or(clamp)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(SystemTime::from)
}

/// How long ago `then` was, in the coarsest unit that fits: `12s ago`, `3m ago`, `2h ago`,
/// `5d ago`. A `then` in the future reads as `0s ago`.
pub(crate) fn age_text(now: SystemTime, then: SystemTime) -> String {
    elapsed_text(now.duration_since(then).unwrap_or_default())
}

/// `age_text` for a span already in hand, as from an `Instant`.
pub(crate) fn elapsed_text(elapsed: Duration) -> String {
    format!("{} ago", span_text(elapsed))
}

/// A span in the coarsest unit that fits: `45s`, `12m`, `1h`, `2d`.
pub(crate) fn span_text(span: Duration) -> String {
    match span.as_secs() {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// `0 bytes`, `1 byte`, `512 bytes`, then decimal units: one decimal below ten of a unit
/// (`1.2 KB`), whole above (`16 KB`, `3 MB`), with a `.0` dropped (`2 KB`).
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, f64); 3] = [("GB", 1e9), ("MB", 1e6), ("KB", 1e3)];
    let size = bytes as f64;
    match UNITS.iter().find(|(_, unit)| size >= *unit) {
        None if bytes == 1 => "1 byte".to_string(),
        None => format!("{bytes} bytes"),
        Some((name, unit)) => {
            let scaled = size / unit;
            let shown = if scaled < 10.0 {
                format!("{scaled:.1}")
            } else {
                format!("{scaled:.0}")
            };
            format!("{} {name}", shown.strip_suffix(".0").unwrap_or(&shown))
        }
    }
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn decode_hex(text: &str) -> Option<Vec<u8>> {
    // `from_str_radix` would take a sign, so the digits are checked first.
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Reticulum's destination derivation: the address hash is the truncated SHA-256 of the
/// name hash followed by the identity's address hash.
pub(crate) fn destination_address(
    name_hash: &[u8; request::NAME_HASH_LEN],
    identity: &AddressHash,
) -> AddressHash {
    AddressHash::new_from_hash(&Hash::new(
        Hash::generator()
            .chain_update(name_hash)
            .chain_update(identity.as_slice())
            .finalize()
            .into(),
    ))
}

/// Lower-cased `text` when it is exactly 32 ASCII hex digits. Upstream's hex parser checks
/// byte length only and slices by byte, so it must never see anything this has not passed.
pub(crate) fn canonical_hash(text: &str) -> Option<String> {
    (text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

/// Peer or local text as this node may show or send it: terminal escape sequences stripped,
/// every other control character and the line and paragraph separators a space, the
/// invisible formatting characters and variation selectors dropped, trimmed, and cut to
/// `max_chars` characters on a character boundary with no trailing whitespace. Blank text
/// is `None`. Escapes go first so a sequence's own bytes never survive as spaces.
pub(crate) fn display_text(text: &str, max_chars: usize) -> Option<String> {
    let cleaned = display_chars(text);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let capped = match trimmed.char_indices().nth(max_chars) {
        Some((cut, _)) => &trimmed[..cut],
        None => trimmed,
    };
    Some(capped.trim_end().to_string())
}

/// `display_text` before it trims and cuts, for a caller that cleans the runs of one line
/// apart and trims the line as a whole.
pub(crate) fn display_chars(text: &str) -> String {
    sanitize_display_text(text)
        .chars()
        .map(|c| match c {
            '\u{2028}' | '\u{2029}' => ' ',
            c => c,
        })
        .filter(|c| !announce::is_control_or_invisible(*c) && !announce::is_variation_selector(*c))
        .collect()
}

/// Peer text cut for one terminal line, and whether it was cut: a caller that appends a
/// pointer to the full text does so only on `cut`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CappedText {
    pub(crate) text: String,
    pub(crate) cut: bool,
}

/// Ends the kept part of a cut peer line; the wiki quotes it.
pub(crate) const CUT_MARKER: &str = "...";

/// `text` as `display_text` cleans it, whole when it has at most `budget` characters,
/// otherwise ending on a complete word within the budget followed by `CUT_MARKER`. The marker
/// sits outside the budget, like the pointer a caller appends after it. The whole text
/// is cleaned before it is measured, so a control character or escape cannot push the
/// marker off the line. The cut lands at the last whitespace at or before character
/// `budget`; with no whitespace that early it lands at `budget` itself. Periods ending
/// the kept part give way to the marker, so a cut after a sentence never shows four or
/// more dots. Blank text is `None`.
pub(crate) fn capped_peer_text(text: &str, budget: usize) -> Option<CappedText> {
    let clean = display_text(text, usize::MAX)?;
    let Some((end, next)) = clean.char_indices().nth(budget) else {
        return Some(CappedText {
            text: clean,
            cut: false,
        });
    };
    let through_next = &clean[..end + next.len_utf8()];
    let kept = match through_next.rfind(char::is_whitespace) {
        Some(at) => &through_next[..at],
        None => &clean[..end],
    };
    Some(CappedText {
        text: format!(
            "{}{CUT_MARKER}",
            kept.trim_end().trim_end_matches('.').trim_end()
        ),
        cut: true,
    })
}

/// Writes `bytes` to a fresh temp file beside `path`, syncs it, then renames it into
/// place, so a crash mid-write cannot leave a half-written file for the next load to
/// refuse. The temp name is unique per write and opened `create_new`, so a planted link
/// under a guessable name is never followed; a symlink at `path` itself is refused, since
/// renaming over it would replace the link while a write through it would land wherever
/// it points. The workspace share list lives in the repository, so a clone chooses what
/// is at these names.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomically_with(path, bytes, false)
}

/// `write_atomically` for a file only its owner may read: the temp file is created
/// `0o600` on unix, so the bytes are never readable by others, not even between the
/// write and the rename. Elsewhere the mode is the platform's.
pub(crate) fn write_atomically_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomically_with(path, bytes, true)
}

fn write_atomically_with(path: &Path, bytes: &[u8], owner_only: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
    }
    refuse_symlink(path)?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = path.with_added_extension(format!("{}-{nanos}.tmp", std::process::id()));
    let mut open = fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if owner_only {
            open.mode(0o600);
        }
    }
    #[cfg(not(unix))]
    let _ = owner_only;
    let mut file = open
        .open(&tmp)
        .with_context(|| format!("Failed to write '{}'", path.display()))?;
    let flushed = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let written = flushed.and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("Failed to write '{}'", path.display()))
}

/// Mesh state is written only to regular files: a symlink at `path` is refused rather
/// than replaced or written through. A missing file passes.
pub(crate) fn refuse_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "'{}' is a symlink; mesh state is written only to regular files. Replace the link with the file it points to and try again.",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("Failed to read metadata of '{}'", path.display()))
        }
    }
}

/// `dunce::canonicalize`, with the `\\?\` prefix that dunce keeps on a Windows path
/// longer than MAX_PATH dropped as well. A share root and a file deep beneath it must
/// resolve to the same spelling or no prefix check between them can hold, and the
/// standard library re-applies the prefix itself whenever a path needs it, so nothing
/// below is lost. The prefix is kept whenever dunce would keep it for another reason:
/// a reserved device name or a name the legacy form would trim. One edge stays closed:
/// a single name of 254 or 255 characters is too long for the probe below on its own,
/// so a path holding one keeps the prefix and fails the root check as before.
pub(crate) fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = dunce::canonicalize(path)?;
    #[cfg(windows)]
    let resolved = without_verbatim_disk(resolved);
    Ok(resolved)
}

#[cfg(windows)]
fn without_verbatim_disk(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    if !matches!(
        components.next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
    ) {
        return path;
    }
    // dunce's per-name rules are reused by asking it about each name alone, where length
    // cannot be the reason it keeps the prefix.
    let names_are_plain = components.all(|component| match component {
        Component::RootDir => true,
        Component::Normal(name) => !dunce::simplified(&Path::new(r"\\?\C:\").join(name))
            .as_os_str()
            .as_encoded_bytes()
            .starts_with(br"\\?\"),
        _ => false,
    });
    if !names_are_plain {
        return path;
    }
    let plain = path
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .map(PathBuf::from);
    plain.unwrap_or(path)
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::{TempDir, read_source, rust_sources};
    use super::*;

    #[test]
    fn mesh_cache_dir_is_mesh_under_cache_dir() {
        assert_eq!(
            mesh_cache_dir(Path::new("/tmp/cache")),
            PathBuf::from("/tmp/cache/mesh")
        );
    }

    #[test]
    fn mesh_config_dir_is_mesh_under_config_dir() {
        assert_eq!(
            mesh_config_dir(Path::new("/tmp/config")),
            PathBuf::from("/tmp/config/mesh")
        );
    }

    #[test]
    fn sizes_read_in_bytes_then_decimal_units() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1), "1 byte");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(999), "999 bytes");
        assert_eq!(human_size(1_200), "1.2 KB");
        assert_eq!(human_size(2_048), "2 KB");
        assert_eq!(human_size(16_384), "16 KB");
        assert_eq!(human_size(3_000_000), "3 MB");
        assert_eq!(human_size(2_500_000_000), "2.5 GB");
    }

    #[test]
    fn display_text_drops_the_unicode_line_and_paragraph_separators() {
        assert_eq!(
            display_text("one\u{2028}two\u{2029}three", 100).as_deref(),
            Some("one two three")
        );
    }

    /// `count` four-letter words separated by spaces: `5 * count - 1` characters.
    fn words(count: usize) -> String {
        (0..count)
            .map(|i| format!("w{i:03}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `count` eleven-letter words separated by spaces: `12 * count - 1` characters, so
    /// character 160 falls inside the fourteenth word and a cut on the budget alone
    /// would split it.
    fn long_words(count: usize) -> String {
        (0..count)
            .map(|i| format!("word{i:07}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn capped_peer_text_cuts_long_text_on_a_word_and_marks_it() {
        let source = long_words(30);
        assert_eq!(source.chars().count(), 359);
        assert_eq!(source.chars().nth(160), Some('0'));
        let capped = capped_peer_text(&source, 160).unwrap();
        assert!(capped.cut);
        let kept = capped
            .text
            .strip_suffix("...")
            .expect("ends with the marker");
        assert!(!kept.ends_with(char::is_whitespace), "{kept:?}");
        assert!(kept.chars().count() < 160, "{kept:?}");
        assert!(source.starts_with(kept), "{kept:?}");
        assert_eq!(
            source[kept.len()..].chars().next(),
            Some(' '),
            "the cut lands on whitespace in the source"
        );
        assert_eq!(kept, long_words(13));
        assert_eq!(kept.chars().count(), 155);
    }

    #[test]
    fn capped_peer_text_keeps_text_within_the_budget_whole() {
        let source = format!("{}x", words(32));
        assert_eq!(source.chars().count(), 160);
        assert_eq!(
            capped_peer_text(&source, 160),
            Some(CappedText {
                text: source.clone(),
                cut: false
            })
        );
        assert!(capped_peer_text("  \u{1b}[2J ", 160).is_none());
    }

    #[test]
    fn capped_peer_text_hard_cuts_a_run_with_no_whitespace() {
        let capped = capped_peer_text(&"q".repeat(300), 160).unwrap();
        assert_eq!(capped.text, format!("{}...", "q".repeat(160)));
        assert!(capped.cut);
    }

    #[test]
    fn capped_peer_text_cuts_at_a_space_sitting_exactly_on_the_budget() {
        let capped = capped_peer_text("abcd efgh", 4).unwrap();
        assert_eq!(capped.text, "abcd...");
        let capped = capped_peer_text("abcd  efgh", 5).unwrap();
        assert_eq!(capped.text, "abcd...");
    }

    #[test]
    fn capped_peer_text_cleans_before_it_measures_so_the_marker_stays_terminal() {
        // The escape is stripped without a space, so the last two runs fuse into one
        // word of 100 that no longer fits beside the first two.
        let source = format!(
            "{}\r{}\u{2028}{}\u{1b}[2J{}",
            "a".repeat(50),
            "b".repeat(50),
            "c".repeat(50),
            "d".repeat(50)
        );
        let capped = capped_peer_text(&source, 160).unwrap();
        assert_eq!(
            capped.text,
            format!("{} {}...", "a".repeat(50), "b".repeat(50))
        );
        assert!(capped.cut);
    }

    mod sentence_cut_tests {
        use super::*;

        #[test]
        fn a_cut_after_a_sentence_ends_in_three_dots_not_four() {
            let sentence = format!("{}.", long_words(13));
            let source = format!("{sentence} {}", long_words(10));
            let capped = capped_peer_text(&source, 160).unwrap();
            assert!(capped.cut);
            assert_eq!(capped.text, format!("{}...", long_words(13)));
            assert!(!capped.text.contains("...."), "{:?}", capped.text);
            let capped = capped_peer_text("abcd... efgh", 8).unwrap();
            assert_eq!(capped.text, "abcd...");
        }

        #[test]
        fn a_period_after_a_space_leaves_no_space_before_the_marker() {
            let capped = capped_peer_text("abcd . efghij", 7).unwrap();
            assert!(capped.cut);
            assert_eq!(capped.text, "abcd...");
        }
    }

    /// `untrusted_content::wrap` keeps its own invisible-character list rather than
    /// importing this module's; this ties the two together so a character
    /// `display_text` drops can never lead a marker line past the fence's quote.
    #[test]
    fn every_invisible_character_display_text_drops_cannot_lead_a_marker_past_the_fence() {
        let dropped = ('\0'..=char::MAX).filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{2028}' | '\u{2029}')
                && (announce::is_control_or_invisible(*c) || announce::is_variation_selector(*c))
        });
        for c in dropped {
            let fenced = crate::utils::untrusted_content::wrap("peer ab12", &format!("{c}=== x"));
            assert!(
                fenced.contains(&format!("\n> {c}=== x\n")),
                "U+{:04X} led an unquoted marker line: {fenced}",
                c as u32
            );
        }
    }

    #[test]
    fn rfc3339_round_trips_to_the_second() {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        let text = rfc3339_utc(time);
        assert_eq!(text, "2026-09-21T14:13:20Z");
        assert_eq!(parse_rfc3339(&text), Some(time));
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn rfc3339_renders_every_system_time_without_failing() {
        use std::time::Duration;
        // SystemTime's range is platform-specific (Windows holds 100 ns ticks in an
        // i64), so the far legs only run where the input itself is representable.
        let far_future = [u64::MAX / 2, 9_000_000_000_000]
            .into_iter()
            .filter_map(|secs| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs)));
        for time in far_future {
            assert_eq!(
                rfc3339_utc(time),
                "+262142-12-31T23:59:59Z",
                "a time past the calendar's end reads as that end"
            );
        }
        assert_eq!(
            rfc3339_utc(SystemTime::UNIX_EPOCH - Duration::from_millis(1_500)),
            "1969-12-31T23:59:58Z",
            "a time before the epoch is floored to its second"
        );
        if let Some(far_past) =
            SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(u64::MAX / 2))
        {
            assert_eq!(rfc3339_utc(far_past), "-262143-01-01T00:00:00Z");
        }
    }

    #[test]
    fn hex_lower_is_lowercase_zero_padded() {
        assert_eq!(hex_lower(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn decode_hex_takes_hex_digit_pairs_and_nothing_else() {
        assert_eq!(decode_hex("00abFF"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(decode_hex(""), Some(vec![]));
        for text in ["abc", "+1", "-1", "0x", "zz", "a b", "é1"] {
            assert_eq!(decode_hex(text), None, "{text:?}");
        }
    }

    #[test]
    fn write_atomically_leaves_no_temp_file_and_replaces_content() {
        let tmp = TempDir::new("write-atomically");
        let path = tmp.path.join("nested").join("state.json");

        write_atomically(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");

        write_atomically(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        let blocked = tmp.path.join("file-not-dir");
        fs::write(&blocked, b"").unwrap();
        let err = write_atomically(&blocked.join("x.yaml"), b"x").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(&blocked.display().to_string()), "{text}");
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(umask)]
    fn the_owner_only_write_yields_a_0600_file_and_the_plain_write_the_platform_mode() {
        use std::os::unix::fs::PermissionsExt;
        let _umask = crate::testing::UmaskGuard::zero();
        let tmp = TempDir::new("write-atomically-owner-only");
        let mode_of = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        let private = tmp.path.join("private.yaml");
        write_atomically_owner_only(&private, b"first").unwrap();
        assert_eq!(fs::read(&private).unwrap(), b"first");
        assert_eq!(mode_of(&private), 0o600);
        write_atomically_owner_only(&private, b"second").unwrap();
        assert_eq!(fs::read(&private).unwrap(), b"second");
        assert_eq!(mode_of(&private), 0o600);

        let shared = tmp.path.join("shared.yaml");
        write_atomically(&shared, b"open").unwrap();
        assert_eq!(fs::read(&shared).unwrap(), b"open");
        assert_eq!(
            mode_of(&shared),
            0o666,
            "the plain write is left to the umask"
        );
        assert_eq!(
            fs::read_dir(&tmp.path).unwrap().count(),
            2,
            "no temp is left"
        );
    }

    /// A clone can plant whatever it likes beside the workspace share list, so a link at
    /// the old temp name must not be written through and a link at the destination must
    /// not be replaced.
    #[cfg(unix)]
    #[test]
    fn write_atomically_never_follows_a_planted_symlink_at_the_temp_or_the_destination() {
        let tmp = TempDir::new("write-atomically-symlink");
        let sentinel = tmp.path.join("sentinel");
        fs::write(&sentinel, b"untouched").unwrap();
        let dir = tmp.path.join("state");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shares.yaml");
        std::os::unix::fs::symlink(&sentinel, path.with_added_extension("tmp")).unwrap();

        write_atomically(&path, b"written").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"written");
        assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["shares.yaml", "shares.yaml.tmp"],
            "the planted link stays as found and no temp of ours is left"
        );

        let linked = dir.join("linked.yaml");
        std::os::unix::fs::symlink(&sentinel, &linked).unwrap();
        let err = write_atomically(&linked, b"written")
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "'{}' is a symlink; mesh state is written only to regular files. Replace the link with the file it points to and try again.",
                linked.display()
            )
        );
        assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
        assert!(
            fs::symlink_metadata(&linked)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3, "no temp is left");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_relay_binds_once_a_port_still_held_at_its_first_try_is_released() {
        use super::test_support::{TransportRelay, closed_port};
        let port = closed_port().await;
        let holder = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let relay = tokio::spawn(TransportRelay::start_on(port));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(holder);
        let relay = relay
            .await
            .expect("the relay comes up once the port is free");
        assert_eq!(relay.port, port);
        relay.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_relay_gives_up_on_a_port_held_for_its_whole_bind_budget() {
        use super::test_support::{TransportRelay, closed_port};
        let port = closed_port().await;
        let holder = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let Err(err) = tokio::spawn(TransportRelay::start_on(port)).await else {
            panic!("a port never released exhausts the bind budget")
        };
        drop(holder);
        assert!(err.is_panic());
        let text = err.into_panic();
        let text = text
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| text.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap();
        let expected = format!(
            "the relay's first bind of 127.0.0.1:{port} failed after 11 attempts: Address already in use"
        );
        assert!(text.starts_with(&expected), "{text}");
    }

    #[test]
    fn canonicalize_agrees_with_dunce_on_a_short_path() {
        let tmp = TempDir::new("canonicalize-short");
        let file = tmp.path.join("a.md");
        fs::write(&file, b"a").unwrap();

        assert_eq!(
            canonicalize(&file).unwrap(),
            dunce::canonicalize(&file).unwrap()
        );
        assert!(canonicalize(&tmp.path.join("missing")).is_err());
    }

    /// The verbatim prefix dunce keeps for length alone goes; the one it keeps for a
    /// name goes nowhere, and a path without one is untouched.
    #[cfg(windows)]
    #[test]
    fn without_verbatim_disk_drops_the_prefix_only_when_length_was_the_reason() {
        let deep = format!(r"\\?\C:\{}\{}\a.md", "a".repeat(200), "b".repeat(200));
        assert!(deep.len() > 260);
        assert_eq!(
            without_verbatim_disk(PathBuf::from(&deep)),
            PathBuf::from(&deep[4..])
        );
        assert_eq!(
            without_verbatim_disk(PathBuf::from(r"\\?\C:\x\a.md")),
            PathBuf::from(r"C:\x\a.md")
        );

        for kept in [r"\\?\C:\x\CON\y", r"\\?\C:\x\name.\y", r"\\?\C:\x\name \y"] {
            assert_eq!(
                without_verbatim_disk(PathBuf::from(kept)),
                PathBuf::from(kept)
            );
        }

        for plain in [
            r"C:\x\a.md",
            r"\\server\share\a.md",
            r"\\?\UNC\server\share\a.md",
        ] {
            assert_eq!(
                without_verbatim_disk(PathBuf::from(plain)),
                PathBuf::from(plain)
            );
        }
    }

    /// A single name of 254 or 255 characters pushes the per-name probe itself past
    /// MAX_PATH, so dunce keeps the prefix for the name alone and the path keeps its
    /// prefix here: the edge fails closed rather than resolving to a second spelling.
    #[cfg(windows)]
    #[test]
    fn without_verbatim_disk_keeps_the_prefix_for_a_name_the_probe_cannot_measure() {
        for len in [254, 255] {
            let kept = format!(r"\\?\C:\x\{}\a.md", "n".repeat(len));
            assert_eq!(
                without_verbatim_disk(PathBuf::from(&kept)),
                PathBuf::from(&kept),
                "{len}"
            );
        }
        let stripped = format!(r"\\?\C:\x\{}\a.md", "n".repeat(253));
        assert_eq!(
            without_verbatim_disk(PathBuf::from(&stripped)),
            PathBuf::from(&stripped[4..])
        );
    }

    /// A verbatim path takes `.` and `..` literally, so one holding either keeps its
    /// prefix: stripping it would let the plain form resolve them away.
    #[cfg(windows)]
    #[test]
    fn without_verbatim_disk_keeps_the_prefix_around_dot_components() {
        let deep = "d".repeat(300);
        for kept in [
            format!(r"\\?\C:\x\..\{deep}\a.md"),
            format!(r"\\?\C:\x\.\{deep}\a.md"),
            r"\\?\C:\x\..\a.md".to_string(),
            r"\\?\C:\x\.\a.md".to_string(),
        ] {
            assert_eq!(
                without_verbatim_disk(PathBuf::from(&kept)),
                PathBuf::from(&kept),
                "{kept}"
            );
        }
    }

    /// A deep candidate on another drive comes out plain and is still no descendant of a
    /// root on this one: the prefix check stays a prefix check on the drive letter too.
    #[cfg(windows)]
    #[test]
    fn without_verbatim_disk_keeps_another_drive_outside_the_root() {
        let deep = format!(r"{}\{}", "a".repeat(200), "b".repeat(200));
        let candidate = without_verbatim_disk(PathBuf::from(format!(r"\\?\D:\{deep}\a.md")));
        let root = PathBuf::from(format!(r"C:\{deep}"));
        assert_eq!(candidate, PathBuf::from(format!(r"D:\{deep}\a.md")));
        assert_ne!(candidate, root);
        assert!(!candidate.starts_with(&root));
    }

    #[test]
    fn mesh_module_never_names_the_request_ctx() {
        let sources = rust_sources();
        // Assembled at runtime so this test's own text does not match the probes.
        let needles = [
            ["Request", "Context"].concat(),
            ["request", "_context"].concat(),
            ["crate::config::", "request", "_context"].concat(),
        ];
        for path in &sources {
            let source = fs::read_to_string(path).unwrap();
            for needle in &needles {
                assert!(
                    !source.contains(needle),
                    "{} must not reference {needle}",
                    path.display()
                );
            }
        }
        assert!(
            sources.len() >= 6,
            "expected the mesh sources, found {}",
            sources.len()
        );
        for name in ["snapshot.rs", "node.rs"] {
            assert!(
                sources
                    .iter()
                    .any(|path| path.file_name().is_some_and(|file| file == name)),
                "{name} must be among the scanned mesh sources"
            );
        }
    }

    /// The wire identifiers before the SCOPE rename (announce magic, destination
    /// application and aspect, LXMF type tags) must not survive anywhere under `src/mesh`
    /// nor in the mesh-facing files outside it that the redaction scan covers, the one
    /// permitted form being the foreign-magic decode vector that asserts they are refused.
    /// A status-card repo named after this program is not a wire identifier.
    #[test]
    fn no_source_under_mesh_spells_the_pre_scope_wire_identifiers() {
        let mut sources = rust_sources();
        let mesh = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("mesh");
        sources.extend(
            redaction_scan_sources()
                .into_iter()
                .filter(|path| !path.starts_with(&mesh)),
        );
        /// Whether a line spelling the needle is one of the permitted forms.
        type Allowed = fn(&str) -> bool;
        fn never(_: &str) -> bool {
            false
        }
        fn refused_foreign_magic(line: &str) -> bool {
            line.contains("::decode(") && line.contains("), None)")
        }
        fn card_repo_name(line: &str) -> bool {
            line.contains("\"repo\"") || line.contains("repo(") || line.contains("(\"name\"")
        }
        // Needles assembled at runtime so this test's own text does not match them.
        let needles: [(String, Allowed); 5] = [
            (["COY", "M"].concat(), refused_foreign_magic),
            (["\"coy", "ote\""].concat(), card_repo_name),
            (["coy", "ote.mesh"].concat(), never),
            (["coy", "ote.peer/"].concat(), never),
            (["coy", "ote.knock/"].concat(), never),
        ];
        let mut hits = Vec::new();
        for path in &sources {
            for (index, line) in fs::read_to_string(path).unwrap().lines().enumerate() {
                for (needle, allowed) in &needles {
                    if line.contains(needle.as_str()) && !allowed(line) {
                        hits.push(format!(
                            "{}:{}: spells {needle:?}",
                            path.display(),
                            index + 1
                        ));
                    }
                }
            }
        }
        assert!(hits.is_empty(), "{}", hits.join("\n"));
        for expected in ["announce.rs", "conformance/interop.rs", "repl/mesh.rs"] {
            assert!(
                sources.iter().any(|path| path.ends_with(expected)),
                "{expected} must be among the scanned sources"
            );
        }
    }

    /// A staged file is the peer's and stays until a person removes it: no production line
    /// under `src/mesh`, nor in the mesh-facing tool and REPL files, removes a file or
    /// directory it names through the inbox. The inbox's own removal is of its temp file,
    /// and the REPL's one removal is the human's `.mesh inbox --purge-files`, of exactly
    /// this instance's directory after a confirm.
    #[test]
    fn no_mesh_source_removes_an_inbox_path() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<PathBuf> = rust_sources()
            .into_iter()
            .filter(|path| {
                !path.components().any(|c| c.as_os_str() == "conformance")
                    && path.file_name().is_some_and(|name| name != "tests.rs")
            })
            .collect();
        sources.push(src.join("function").join("mesh.rs"));
        sources.push(src.join("repl").join("mesh.rs"));
        let removals = ["remove_file(", "remove_dir_all(", "remove_dir("];
        let inbox_tokens = ["inbox", "Inbox", ".staged", "Part::File"];
        let mut hits = Vec::new();
        let mut inbox_removals = Vec::new();
        let mut repl_removals = Vec::new();
        for path in &sources {
            for (index, line) in production_code(&read_source(path)).iter().enumerate() {
                if !removals.iter().any(|call| line.contains(call)) {
                    continue;
                }
                if path.ends_with("inbox.rs") {
                    inbox_removals.push(line.trim().to_string());
                } else if path.ends_with("repl/mesh.rs") {
                    repl_removals.push(line.trim().to_string());
                } else if inbox_tokens.iter().any(|token| line.contains(token)) {
                    hits.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
        assert_eq!(hits, Vec::<String>::new());
        assert_eq!(
            inbox_removals,
            ["let _ = fs::remove_file(&tmp);"],
            "inbox.rs removes only its own temp file"
        );
        assert_eq!(
            repl_removals,
            ["fs::remove_dir_all(inbox_root)"],
            "the REPL removes only the purge's inbox root"
        );
    }

    /// What the person at the keyboard or the model reads is worded for them, never in
    /// the code's own names: no trust rule variant, no bare refusal byte, none of the
    /// doc comments' coinages. Scanned: every string literal in the production code of
    /// the REPL and tool files, and every literal inside a `Display` impl under
    /// `src/mesh`, since those are the error texts both surfaces print. A hex byte in
    /// parentheses after its name, as the request errors print theirs, is a code beside
    /// a word and passes; a bare one does not. That exempted literal doubles as the proof
    /// the `Display` walk reads real impls. Idle and notification lines built outside a
    /// `Display` impl under `src/mesh` (`access_text`, `not_shared_sentence`,
    /// `record_human_reply`) are not scanned here, and neither are the remedy helpers
    /// the impls append (`fields_remedy`, `propagation_remedy`, `request_remedy`,
    /// `refusal_remedy`): they are free functions, so their literals sit outside the
    /// `impl` block this walk brackets. The sweep in `tests/mesh_wiki_docs.rs` reads
    /// those files whole, with only their test modules blanked, and so covers both.
    #[test]
    fn no_user_facing_mesh_string_carries_banned_vocabulary() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let words = [
            ["Default", "Closed"].concat(),
            ["record", "-less"].concat(),
            ["told", " apart"].concat(),
        ];
        let hex_prefix = ["0", "x"].concat();
        let names_a_code = |before: &str| {
            before
                .strip_suffix(" (")
                .is_some_and(|head| head.chars().last().is_some_and(|c| c != ' '))
        };
        let bare_hex = |literal: &str| {
            literal
                .match_indices(&hex_prefix)
                .any(|(at, _)| !names_a_code(&literal[..at]))
        };
        let is_hit =
            |literal: &str| words.iter().any(|word| literal.contains(word)) || bare_hex(literal);
        let code_in_parens = ["{name} (", hex_prefix.as_str(), "{:02x})"].concat();
        let flagged = [
            format!("x {} y", words[0]),
            words[1].clone(),
            format!("not {}", words[2]),
            format!("code {hex_prefix}1f"),
        ];
        let fixture: Vec<String> = flagged
            .iter()
            .chain([&code_in_parens])
            .map(|literal| format!("    write!(f, \"{literal}\")"))
            .collect();
        let control: Vec<(usize, String)> = literals_by_line(&fixture)
            .into_iter()
            .filter(|(_, literal)| is_hit(literal))
            .collect();
        let expected: Vec<(usize, String)> = flagged
            .iter()
            .enumerate()
            .map(|(index, literal)| (index + 1, literal.clone()))
            .collect();
        assert_eq!(
            control, expected,
            "the hit predicate flags each banned form once"
        );
        let mut seen = Vec::new();
        let mut scan = |path: &Path, first_line: usize, lines: &[String]| {
            for (line, literal) in literals_by_line(lines) {
                seen.push((
                    format!("{}:{}", path.display(), first_line + line - 1),
                    literal,
                ));
            }
        };
        for path in [
            src.join("repl").join("mesh.rs"),
            src.join("function").join("mesh.rs"),
        ] {
            scan(&path, 1, &production_code(&read_source(&path)));
        }
        let display_headers = ["impl fmt::Display for ", "impl std::fmt::Display for "];
        for path in rust_sources() {
            let lines = production_code(&read_source(&path));
            let mut at = 0;
            while at < lines.len() {
                let opens_display = display_headers
                    .iter()
                    .any(|header| lines[at].starts_with(header));
                if !opens_display {
                    at += 1;
                    continue;
                }
                let rest = lines[at..].join("\n");
                let open = rest.find('{').unwrap();
                let close = matching_bracket(&rest, open, (b'{', b'}'))
                    .unwrap_or_else(|| panic!("{}:{}: no closing brace", path.display(), at + 1));
                let end = at + rest[..=close].matches('\n').count();
                scan(&path, at + 1, &lines[at..=end]);
                at = end + 1;
            }
        }
        let hits: Vec<String> = seen
            .iter()
            .filter(|(_, literal)| is_hit(literal))
            .map(|(at, literal)| format!("{at}: {literal}"))
            .collect();
        assert_eq!(hits, Vec::<String>::new());
        assert!(
            seen.iter().any(|(_, literal)| *literal == code_in_parens),
            "the request errors' Display impl was not scanned"
        );
    }

    #[test]
    fn announce_doc_comments_name_the_scope_node() {
        let announce = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("mesh")
            .join("announce.rs");
        let text = fs::read_to_string(&announce).unwrap();
        for expected in ["SCOPE node", "SCOPE announce"] {
            assert!(text.contains(expected), "announce.rs must say {expected:?}");
        }
        for stale in ["Coyote node", "Coyote announce"] {
            assert!(!text.contains(stale), "announce.rs still says {stale:?}");
        }
    }

    /// Words a log line may not interpolate or pass as an argument. The list mirrors
    /// the log rule: "a peer's message content or title, its `fields`, a knock introduction,
    /// an envoy question or answer, a status card or any text of one, nor the human's brief,
    /// objective or session name".
    const UNLOGGABLE_WORDS: [&str; 17] = [
        "content",
        "body",
        "brief",
        "objective",
        "session_name",
        "title",
        "fields",
        "intro",
        "question",
        "card",
        "hint",
        "text",
        "answer",
        "reply",
        "reply_text",
        "alias",
        "person_name",
    ];
    /// Identifiers that carry a full identity or destination hash; each must be inside
    /// `short(` where it is logged.
    const HASH_WORDS: [&str; 11] = [
        "fingerprint",
        "hash",
        "destination_hash",
        "destination_hex",
        "identity_hex",
        "identity_hash",
        "address_hash",
        "source_identity",
        "source_destination",
        "identity",
        "destination",
    ];
    /// Calls that render a hash in full; the receiver or first argument decides.
    const HASH_CALLS: [&str; 2] = [".to_hex_string()", "hex_lower("];
    /// Receivers whose hex is a per-link, per-request or per-message identifier, which
    /// the redaction rule lets a log line carry in full.
    const FULL_HEX_RECEIVERS: [&str; 6] = [
        "link_id",
        "link",
        "request_id",
        "transient",
        "transient_id",
        "message_id",
    ];
    /// Names an error value goes by. The scan cannot look through an error's Display, so
    /// where one of these is interpolated or passed it must sit inside `redact_hashes(`,
    /// which is what the redaction rule asks of every sink that logs an error's text.
    const ERROR_WORDS: [&str; 6] = ["err", "e", "error", "why", "cause", "failure"];
    const LOG_MACROS: [&str; 5] = ["debug!(", "trace!(", "info!(", "warn!(", "error!("];

    /// Files the scan reads selectively: a file listed here is scanned only at the log
    /// invocations whose string literal mentions one of its markers, so a shared file's
    /// unrelated sinks are not swept in. The REPL completion file under config hosts the
    /// `.mesh` completion helpers among sinks the redaction rule does not govern; its name is
    /// assembled at runtime because the mesh module must not spell it out.
    fn literal_filtered_sources() -> Vec<(PathBuf, &'static [&'static str])> {
        let completion_file = Path::new("config").join(["request", "_context.rs"].concat());
        vec![(completion_file, &[".mesh", "mesh_completion"])]
    }

    /// The literal-filter markers of `path`, if it is a literal-filtered file.
    fn literal_markers(path: &Path) -> Option<&'static [&'static str]> {
        literal_filtered_sources()
            .into_iter()
            .find_map(|(suffix, markers)| path.ends_with(&suffix).then_some(markers))
    }

    /// The files the redaction rule governs: the mesh module without its test-only files, and the
    /// mesh-facing files under config, function and repl, plus the literal-filtered files.
    fn redaction_scan_sources() -> Vec<PathBuf> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<PathBuf> = rust_sources()
            .into_iter()
            .filter(|path| {
                !path.components().any(|c| c.as_os_str() == "conformance")
                    && path.file_name().is_some_and(|name| name != "tests.rs")
            })
            .collect();
        for entry in fs::read_dir(src.join("config")).unwrap() {
            let path = entry.unwrap().path();
            let is_mesh_file = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mesh_") && name.ends_with(".rs"));
            if is_mesh_file {
                sources.push(path);
            }
        }
        sources.push(src.join("function").join("mesh.rs"));
        sources.push(src.join("repl").join("mesh.rs"));
        for (suffix, _) in literal_filtered_sources() {
            sources.push(src.join(suffix));
        }
        sources
    }

    /// Whether the scan reads `invocation` found in `path`: every invocation of an
    /// unfiltered file, and of a literal-filtered file only those whose string literal
    /// mentions one of the file's markers.
    fn in_scan(path: &Path, invocation: &str) -> bool {
        let Some(markers) = literal_markers(path) else {
            return true;
        };
        split_literals(invocation)
            .iter()
            .any(|(_, literal)| markers.iter().any(|marker| literal.contains(marker)))
    }

    /// `source` line for line, so line numbers hold, with every `#[cfg(test)]`-attributed
    /// inline module blanked and `//` comments removed, a `//` inside a string literal kept.
    /// Raw string literals are read as plain ones first (`plain_strings`).
    fn production_code(source: &str) -> Vec<String> {
        let source = plain_strings(source);
        let mut lines: Vec<String> = source
            .lines()
            .map(|line| strip_line_comment(line).to_string())
            .collect();
        let mut at = 0;
        while at + 1 < lines.len() {
            let opener = &lines[at + 1];
            let opens_test_module = lines[at] == "#[cfg(test)]"
                && opener.trim_end().ends_with('{')
                && (opener.starts_with("mod ") || opener.starts_with("pub(crate) mod "));
            if !opens_test_module {
                at += 1;
                continue;
            }
            let rest = lines[at + 1..].join("\n");
            let open = rest.find('{').unwrap();
            let close = matching_bracket(&rest, open, (b'{', b'}'))
                .unwrap_or_else(|| panic!("line {}: no closing brace for the test module", at + 2));
            let end = at + 1 + rest[..=close].matches('\n').count();
            for line in &mut lines[at..=end] {
                line.clear();
            }
            at = end + 1;
        }
        lines
    }

    /// `source` with every raw string literal (`r"..."`, `r#"..."#`, `br"..."`) rewritten as
    /// a plain literal of the same length: the `r` and the `#`s become spaces, and inside the
    /// body a `"` becomes `'` and a `\` becomes `/`, so the quote-and-escape scanners below
    /// read it as one string. Line breaks are kept, so line numbers hold.
    fn plain_strings(source: &str) -> String {
        let bytes = source.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut in_string = false;
        let mut at = 0;
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        while at < bytes.len() {
            let b = bytes[at];
            if in_string {
                if b == b'\\' && at + 1 < bytes.len() {
                    out.extend_from_slice(&bytes[at..at + 2]);
                    at += 2;
                    continue;
                }
                in_string = b != b'"';
            } else if b == b'\''
                && bytes.get(at + 1) == Some(&b'\\')
                && bytes.get(at + 3) == Some(&b'\'')
            {
                out.extend_from_slice(&bytes[at..at + 4]);
                at += 4;
                continue;
            } else if b == b'\'' && bytes.get(at + 2) == Some(&b'\'') {
                out.extend_from_slice(&bytes[at..at + 3]);
                at += 3;
                continue;
            } else if b == b'"' {
                in_string = true;
            } else if b == b'r'
                && at.checked_sub(1).is_none_or(|p| {
                    !is_ident(bytes[p])
                        || (matches!(bytes[p], b'b' | b'c')
                            && p.checked_sub(1).is_none_or(|q| !is_ident(bytes[q])))
                })
            {
                let hashes = bytes[at + 1..].iter().take_while(|b| **b == b'#').count();
                if bytes.get(at + 1 + hashes) == Some(&b'"') {
                    let body = at + 2 + hashes;
                    let mut closer = vec![b'#'; hashes];
                    closer.insert(0, b'"');
                    let Some(end) = (body..bytes.len()).find(|&i| bytes[i..].starts_with(&closer))
                    else {
                        break;
                    };
                    out.extend(vec![b' '; 1 + hashes]);
                    out.push(b'"');
                    out.extend(bytes[body..end].iter().map(|b| match b {
                        b'"' => b'\'',
                        b'\\' => b'/',
                        other => *other,
                    }));
                    out.push(b'"');
                    out.extend(vec![b' '; hashes]);
                    at = end + 1 + hashes;
                    continue;
                }
            }
            out.push(b);
            at += 1;
        }
        out.extend_from_slice(&bytes[at..]);
        String::from_utf8(out).unwrap()
    }

    /// `line` up to its first `//` outside a string literal; a `'"'` char literal does not
    /// open one.
    fn strip_line_comment(line: &str) -> &str {
        let bytes = line.as_bytes();
        let mut in_string = false;
        let mut at = 0;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if in_string => at += 1,
                b'\'' if !in_string && bytes.get(at + 2) == Some(&b'\'') => at += 2,
                b'"' => in_string = !in_string,
                b'/' if !in_string && bytes.get(at + 1) == Some(&b'/') => return &line[..at],
                _ => {}
            }
            at += 1;
        }
        line
    }

    /// Every log macro invocation in `lines` as (1-based line, text from the macro name to
    /// its closing paren), string literals kept. An invocation whose closing paren is not
    /// found is a scanner fault, so it panics rather than scanning a guess.
    fn log_invocations(lines: &[String]) -> Vec<(usize, String)> {
        let joined = lines.join("\n");
        let mut found = Vec::new();
        let mut from = 0;
        while let Some((start, macro_name)) = LOG_MACROS
            .iter()
            .filter_map(|name| joined[from..].find(name).map(|at| (from + at, *name)))
            .min()
        {
            let boundary_before = start == 0
                || !joined.as_bytes()[start - 1].is_ascii_alphanumeric()
                    && joined.as_bytes()[start - 1] != b'_';
            if !boundary_before {
                from = start + macro_name.len();
                continue;
            }
            let open = start + macro_name.len() - 1;
            let line = joined[..start].matches('\n').count() + 1;
            let close = matching_paren(&joined, open)
                .unwrap_or_else(|| panic!("line {line}: no closing paren for `{macro_name}`"));
            found.push((line, joined[start..=close].to_string()));
            from = close + 1;
        }
        found
    }

    /// The index of the `)` closing the `(` at `open`.
    fn matching_paren(text: &str, open: usize) -> Option<usize> {
        matching_bracket(text, open, (b'(', b')'))
    }

    /// The index of the `pair.1` closing the `pair.0` at `open`, skipping brackets inside
    /// string and char literals.
    fn matching_bracket(text: &str, open: usize, pair: (u8, u8)) -> Option<usize> {
        let bytes = text.as_bytes();
        let mut depth = 0;
        let mut in_string = false;
        let mut at = open;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if in_string => at += 1,
                b'\'' if !in_string && bytes.get(at + 2) == Some(&b'\'') => at += 2,
                b'"' => in_string = !in_string,
                byte if !in_string && byte == pair.0 => depth += 1,
                byte if !in_string && byte == pair.1 => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(at);
                    }
                }
                _ => {}
            }
            at += 1;
        }
        None
    }

    /// (code, string literal) pairs of an invocation; the last pair's literal is empty.
    fn split_literals(text: &str) -> Vec<(String, String)> {
        let mut parts = Vec::new();
        let mut code = String::new();
        let mut literal = String::new();
        let mut in_string = false;
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' if in_string => {
                    literal.push(c);
                    literal.extend(chars.next());
                }
                '"' if in_string => {
                    parts.push((std::mem::take(&mut code), std::mem::take(&mut literal)));
                    in_string = false;
                }
                '"' => in_string = true,
                _ if in_string => literal.push(c),
                _ => code.push(c),
            }
        }
        parts.push((code, String::new()));
        parts
    }

    /// (1-based line, literal) for every string literal in `lines`, a literal spanning
    /// lines counted at the one it opens on. The `'"'` char literal is blanked first so
    /// `split_literals` does not read it as opening a string.
    fn literals_by_line(lines: &[String]) -> Vec<(usize, String)> {
        let text = lines.join("\n").replace("'\"'", "' '");
        let parts = split_literals(&text);
        let trailing_code = parts.len() - 1;
        let mut line = 1;
        let mut found = Vec::new();
        for (index, (code, literal)) in parts.into_iter().enumerate() {
            line += code.matches('\n').count();
            if index < trailing_code {
                found.push((line, literal.clone()));
            }
            line += literal.matches('\n').count();
        }
        found
    }

    fn is_ident_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }

    fn word_positions(text: &str, word: &str) -> Vec<usize> {
        let bytes = text.as_bytes();
        text.match_indices(word)
            .filter(|(at, _)| {
                at.checked_sub(1)
                    .is_none_or(|before| !is_ident_byte(bytes[before]))
                    && bytes
                        .get(at + word.len())
                        .is_none_or(|after| !is_ident_byte(*after))
            })
            .map(|(at, _)| at)
            .collect()
    }

    /// The argument expression starting at `at`: up to the next `,` or `)` at its own depth.
    fn argument_from(code: &str, at: usize) -> &str {
        let bytes = code.as_bytes();
        let mut depth = 0;
        let mut end = at;
        while end < bytes.len() {
            match bytes[end] {
                b'(' => depth += 1,
                b')' if depth > 0 => depth -= 1,
                b')' => break,
                b',' if depth == 0 => break,
                _ => {}
            }
            end += 1;
        }
        code[at..end].trim()
    }

    /// A tag, an id, a length or a party hash taken from a peer value is a count or a key,
    /// not its text; the hash rules hold the party hash to `short(`.
    fn is_tag_id_or_length(argument: &str) -> bool {
        [
            ".kind",
            ".id",
            ".message_id",
            ".source_identity",
            ".source_destination",
            ".len()",
            "len)",
        ]
        .iter()
        .any(|suffix| argument.ends_with(suffix))
    }

    /// `{name}` and `{name:?}` placeholders of a format string.
    fn placeholders(literal: &str) -> Vec<&str> {
        literal
            .split('{')
            .skip(1)
            .filter_map(|rest| {
                let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
                (rest[end..].starts_with('}') || rest[end..].starts_with(':')).then(|| &rest[..end])
            })
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// Whether any unclosed `(` before `at` is a call of `callee`.
    fn inside_call(code: &str, at: usize, callee: &str) -> bool {
        let bytes = code.as_bytes();
        let mut depth = 0;
        for index in (0..at).rev() {
            match bytes[index] {
                b')' => depth += 1,
                b'(' if depth > 0 => depth -= 1,
                b'(' if code[..index].ends_with(callee) => return true,
                _ => {}
            }
        }
        false
    }

    /// The last identifier segment of the receiver before `.to_hex_string()` at `at`, or of
    /// the first argument after `hex_lower(` at `at`.
    fn hex_subject(code: &str, at: usize, call: &str) -> String {
        let bytes = code.as_bytes();
        if call.starts_with('.') {
            let start = (0..at)
                .rev()
                .find(|index| !(is_ident_byte(bytes[*index]) || bytes[*index] == b'.'))
                .map_or(0, |index| index + 1);
            code[start..at].rsplit('.').next().unwrap_or("").to_string()
        } else {
            code[at + call.len()..]
                .trim_start_matches(['&', ' '])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
                .collect::<String>()
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_string()
        }
    }

    /// The redaction violations in one log invocation.
    fn redaction_violations(invocation: &str) -> Vec<String> {
        let mut violations = Vec::new();
        let mut code_so_far = String::new();
        for (code, literal) in split_literals(invocation) {
            code_so_far.push_str(&code);
            let base = code_so_far.len() - code.len();
            let literal_is_redacted = inside_call(&code_so_far, code_so_far.len(), "redact_hashes");
            for name in placeholders(&literal) {
                if UNLOGGABLE_WORDS.contains(&name) {
                    violations.push(format!("interpolates `{{{name}}}`"));
                }
                if HASH_WORDS.contains(&name) {
                    violations.push(format!("interpolates the full hash `{{{name}}}`"));
                }
                if ERROR_WORDS.contains(&name) && !literal_is_redacted {
                    violations.push(format!(
                        "interpolates the error `{{{name}}}` outside redact_hashes()"
                    ));
                }
            }
            for word in UNLOGGABLE_WORDS {
                for at in word_positions(&code, word) {
                    if !is_tag_id_or_length(argument_from(&code, at)) {
                        violations.push(format!("passes `{word}`"));
                    }
                }
            }
            for word in HASH_WORDS {
                for at in word_positions(&code, word) {
                    if !inside_call(&code, at, "short") {
                        violations.push(format!("passes `{word}` outside short()"));
                    }
                }
            }
            for word in ERROR_WORDS {
                for at in word_positions(&code, word) {
                    let is_macro_name = code.as_bytes().get(at + word.len()) == Some(&b'!');
                    if !is_macro_name && !inside_call(&code_so_far, base + at, "redact_hashes") {
                        violations.push(format!("passes `{word}` outside redact_hashes()"));
                    }
                }
            }
            for call in HASH_CALLS {
                for (at, _) in code.match_indices(call) {
                    let subject = hex_subject(&code, at, call);
                    if !FULL_HEX_RECEIVERS.contains(&subject.as_str())
                        && !inside_call(&code, at, "short")
                    {
                        violations
                            .push(format!("renders `{subject}` with `{call}` outside short()"));
                    }
                }
            }
        }
        violations
    }

    #[test]
    fn redaction_scanner_flags_each_rule_and_passes_the_permitted_forms() {
        let flagged: [(&str, &[&str]); 21] = [
            ("debug!(\"got {content}\")", &["interpolates `{content}`"]),
            ("debug!(\"named {alias}\")", &["interpolates `{alias}`"]),
            (
                "info!(\"card says {}\", card_fields.person_name)",
                &["passes `person_name`"],
            ),
            ("debug!(\"{content:?}\")", &["interpolates `{content}`"]),
            ("debug!(\"got {}\", message.title)", &["passes `title`"]),
            ("debug!(\"sent {}\", answer)", &["passes `answer`"]),
            ("debug!(\"{reply_text}\")", &["interpolates `{reply_text}`"]),
            (
                "warn!(\"knock from {fingerprint}\")",
                &["interpolates the full hash `{fingerprint}`"],
            ),
            (
                "debug!(\"aged out {hash}\")",
                &["interpolates the full hash `{hash}`"],
            ),
            (
                "debug!(\"peer {}\", identity_hex)",
                &["passes `identity_hex` outside short()"],
            ),
            (
                "info!(\"peer {}\", destination.address_hash.to_hex_string())",
                &[
                    "passes `address_hash` outside short()",
                    "passes `destination` outside short()",
                    "renders `address_hash` with `.to_hex_string()` outside short()",
                ],
            ),
            (
                "debug!(\"peer {}\", identity.to_hex_string())",
                &[
                    "passes `identity` outside short()",
                    "renders `identity` with `.to_hex_string()` outside short()",
                ],
            ),
            (
                "debug!(\"id {}\", hex_lower(advertised))",
                &["renders `advertised` with `hex_lower(` outside short()"],
            ),
            (
                "debug!(\n    \"multi {}\",\n    peer.destination_hash\n)",
                &["passes `destination_hash` outside short()"],
            ),
            (
                "debug!(\"{} // {}\", body.kind, message.content)",
                &["passes `content`"],
            ),
            (
                "debug!(\"from {}\", reply.source_identity)",
                &["passes `source_identity` outside short()"],
            ),
            (
                "warn!(\"knock was not cached: {err:#}\")",
                &["interpolates the error `{err}` outside redact_hashes()"],
            ),
            (
                "debug!(\"unreadable: {e:?}\")",
                &["interpolates the error `{e}` outside redact_hashes()"],
            ),
            (
                "error!(\"run failed: {why}\")",
                &["interpolates the error `{why}` outside redact_hashes()"],
            ),
            (
                "warn!(\"could not persist: {}\", err)",
                &["passes `err` outside redact_hashes()"],
            ),
            (
                "warn!(\"{}: {}\", short(&identity_hex), format!(\"{:#}\", cause))",
                &["passes `cause` outside redact_hashes()"],
            ),
        ];
        for (text, violations) in flagged {
            let invocations = log_invocations(&production_code(text));
            assert_eq!(invocations.len(), 1, "{text}");
            assert_eq!(
                redaction_violations(&invocations[0].1),
                violations,
                "{text}"
            );
        }
        let permitted = [
            "debug!(\"peer {}\", short(&identity_hex))",
            "debug!(\"peer {}\", short(&destination.address_hash.to_hex_string()))",
            "debug!(\"resource {}\", short(&crate::mesh::hex_lower(hash.as_slice())))",
            "debug!(\"link {}\", link_id.to_hex_string())",
            "debug!(\"request {} on link {}\", request_id.to_hex_string(), event.link_id.to_hex_string())",
            "debug!(\"transient {}\", hex_lower(transient))",
            "debug!(\"({} bytes) body-free text: {}\", count, len)",
            "debug!(\"{} {}\", body.kind, body.id)",
            "debug!(\"reply {} from {}\", reply.message_id, short(&reply.source_identity))",
            "debug!(\"{} bytes\", message.content.as_ref().map_or(0, Vec::len))",
            "debug!(\"{} chars\", intro.len())",
            "warn!(\"was not cached: {}\", redact_hashes(&format!(\"{err:#}\")))",
            "warn!(\"not persisted: {}\", redact_hashes(&err.to_string()))",
            "error!(\"run for {id} failed: {}\", redact_hashes(err))",
            "warn!(\"refused: {}\", redact_hashes(&why))",
            "debug!(\"{} {}\", short(&identity_hex), redact_hashes(&format!(\"{}: {e:?}\", label)))",
            "warn!(\"{}\", redact_hashes(&format!(\"{}: {}\", label, err)))",
        ];
        for text in permitted {
            let lines = vec![text.to_string()];
            for (_, invocation) in log_invocations(&lines) {
                assert_eq!(
                    redaction_violations(&invocation),
                    Vec::<String>::new(),
                    "{text}"
                );
            }
        }
        for opener in ["mod tests {", "pub(crate) mod x {"] {
            let source = format!(
                "fn a() {{\n    debug!(\"x\");\n}}\n#[cfg(test)]\n{opener}\n    debug!(\"{{content}}\");\n}}\n"
            );
            assert_eq!(
                log_invocations(&production_code(&source)).len(),
                1,
                "{opener}"
            );
        }
        let after_helpers = "#[cfg(test)]\nmod helpers {\n    fn a() {\n        let _ = '{';\n        debug!(\"{content} }}\");\n    }\n}\n\nfn b() {\n    debug!(\"{content}\");\n}\n";
        let invocations = log_invocations(&production_code(after_helpers));
        assert_eq!(
            invocations
                .iter()
                .map(|(line, invocation)| (*line, redaction_violations(invocation)))
                .collect::<Vec<_>>(),
            [(10, vec!["interpolates `{content}`".to_string()])]
        );
        assert_eq!(
            production_code("debug!(\"a // b\"); // debug!(\"{content}\")"),
            ["debug!(\"a // b\"); "]
        );
        assert_eq!(
            production_code("x.replace('\"', \"'\"); // debug!(\"{content}\")"),
            ["x.replace('\"', \"'\"); "]
        );
    }

    #[test]
    #[should_panic(expected = "no closing paren for `debug!(`")]
    fn redaction_scanner_refuses_an_unclosed_log_macro() {
        log_invocations(&["debug!(\"open".to_string()]);
    }

    #[test]
    #[should_panic(expected = "line 2: no closing brace for the test module")]
    fn redaction_scanner_refuses_an_unclosed_test_module() {
        production_code("#[cfg(test)]\nmod helpers {\n    fn a() {}\n");
    }

    #[test]
    fn redaction_scanner_reads_raw_strings_as_plain_ones() {
        let source = concat!(
            "fn a() {\n",
            "    debug!(\"x\");\n",
            "}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    const A: &str = r\"plans\\\";\n",
            "    const B: &str = \"}\";\n",
            "    const C: [&str; 2] = [r#\"a \"quoted\" b\"#, br\"c\\\"];\n",
            "    const D: &str = \"a plain r\\\" stays\";\n",
            "    fn b() {\n",
            "        debug!(\"{content}\");\n",
            "    }\n",
            "}\n",
        );
        assert_eq!(log_invocations(&production_code(source)).len(), 1);
        let plain = plain_strings(source);
        assert_eq!(plain.lines().count(), source.lines().count());
        assert_eq!(plain.len(), source.len());
        assert!(plain.contains(" \"plans/\";"), "{plain}");
        assert!(plain.contains("  \"a 'quoted' b\" "), "{plain}");
        assert!(plain.contains("b \"c/\""), "{plain}");
        assert!(plain.contains("\"a plain r\\\" stays\""), "{plain}");
        assert!(plain.contains("debug!(\"x\")"), "{plain}");
    }

    #[test]
    fn redaction_scan_holds_the_completion_file_to_its_mesh_completion_sinks() {
        let (filtered, _) = literal_filtered_sources().remove(0);
        let filtered = Path::new("src").join(filtered);
        let unfiltered = Path::new("src/mesh/node.rs");
        let source = "fn a() {\n    warn!(\"failed to compute effective role: {err}\");\n    debug!(\"knock cache unreadable while completing `.mesh`: {err:#}\");\n    debug!(\"{}\", mesh_completion_label(err));\n    debug!(\"mesh_completion list unreadable: {}\", err);\n}\n";
        let invocations = log_invocations(&production_code(source));
        assert_eq!(invocations.len(), 4);
        let scanned = |path: &Path| -> Vec<(usize, Vec<String>)> {
            invocations
                .iter()
                .filter(|(_, invocation)| in_scan(path, invocation))
                .map(|(line, invocation)| (*line, redaction_violations(invocation)))
                .collect()
        };
        // In the completion file the non-mesh `{err}` sink (line 2) and the sink whose
        // marker sits in code rather than in a literal (line 4) are outside the scan; the
        // `.mesh` and `mesh_completion` literals (lines 3 and 5) are inside it and flagged.
        assert_eq!(
            scanned(&filtered),
            [
                (
                    3,
                    vec!["interpolates the error `{err}` outside redact_hashes()".to_string()]
                ),
                (5, vec!["passes `err` outside redact_hashes()".to_string()]),
            ]
        );
        // An unfiltered file is scanned at every sink.
        assert_eq!(scanned(unfiltered).len(), 4);
        assert!(
            scanned(unfiltered)
                .iter()
                .all(|(_, violations)| !violations.is_empty())
        );
    }

    #[test]
    fn mesh_log_lines_never_carry_peer_text_or_a_full_hash() {
        let sources = redaction_scan_sources();
        let mut hits = Vec::new();
        let mut repl_invocations = 0;
        let mut completion_file_invocations = 0;
        for path in &sources {
            let source = fs::read_to_string(path).unwrap();
            let invocations: Vec<(usize, String)> = log_invocations(&production_code(&source))
                .into_iter()
                .filter(|(_, invocation)| in_scan(path, invocation))
                .collect();
            if path.ends_with("repl/mesh.rs") {
                repl_invocations = invocations.len();
            }
            if literal_markers(path).is_some() {
                completion_file_invocations = invocations.len();
            }
            for (line, invocation) in invocations {
                for violation in redaction_violations(&invocation) {
                    hits.push(format!("{}:{line}: {violation}", path.display()));
                }
            }
        }
        assert!(
            repl_invocations > 0,
            "the scan must reach the production log line in src/repl/mesh.rs"
        );
        assert!(
            sources
                .iter()
                .any(|path| path.ends_with("config/mesh_envoy.rs")),
            "the scan must reach the mesh-facing config files"
        );
        assert!(
            completion_file_invocations > 0,
            "the scan must reach the `.mesh` completion sinks in the REPL completion file"
        );
        assert_eq!(
            hits,
            Vec::<String>::new(),
            "log lines violating the redaction rule"
        );
    }

    /// A send a peer refused or met with silence is logged without the `<user> /
    /// <session>` name the user is shown, which can be one of the user's own aliases;
    /// the short hash still says which session it was.
    #[test]
    fn a_logged_named_refusal_or_no_answer_carries_no_user_or_session_name() {
        use crate::mesh::message::SendError;
        use crate::mesh::request::{NoAnswer, RemoteRefusal};

        use crate::mesh::access::AccessError;
        use crate::mesh::card::StatusError;
        use crate::mesh::fetch::FetchError;

        let peer = "ab".repeat(16);
        let own = "cd".repeat(16);
        let name = "Alice / laptop";
        let refusal = RemoteRefusal::new(&peer, Some(name), own.clone());
        let silence = NoAnswer::new(&peer, own.clone()).named(Some(name));
        let told_and_logged: Vec<(String, String)> = vec![
            {
                let err = SendError::NotTrustedByPeer(refusal.clone());
                (err.to_string(), err.log_text())
            },
            {
                let err = SendError::NoAnswer(silence.clone());
                (err.to_string(), err.log_text())
            },
            {
                let err = AccessError::NotTrustedByPeer(refusal.clone());
                (err.to_string(), err.log_text())
            },
            {
                let err = AccessError::NoAnswer(silence.clone());
                (err.to_string(), err.log_text())
            },
            {
                let err = FetchError::NotTrustedByPeer(refusal.clone());
                (err.to_string(), err.log_text())
            },
            {
                let err = FetchError::NoAnswer(silence.clone());
                (err.to_string(), err.log_text())
            },
            {
                let err = StatusError::NotTrustedByPeer(refusal.clone());
                (err.to_string(), err.log_text())
            },
            {
                let err = StatusError::NoAnswer(silence.clone());
                (err.to_string(), err.log_text())
            },
        ];
        for (told, logged) in told_and_logged {
            assert!(told.contains(name), "{told}");
            let logged = redact_hashes(&logged);
            assert!(
                !logged.contains("Alice") && !logged.contains("laptop"),
                "{logged}"
            );
            assert!(logged.contains(short(&peer)), "{logged}");
        }
    }
}
