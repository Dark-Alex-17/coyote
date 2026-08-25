//! Content policy for MCP resource and tool content: UTF-8-boundary-safe text
//! paging, grep-style pattern filtering, and spill-to-disk for binary blobs.

use crate::config::paths;
use base64::engine::general_purpose::STANDARD;
use base64::read::DecoderReader;
use fancy_regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use std::{fmt, io};

/// Default page size when the caller does not specify `max_bytes`.
pub const DEFAULT_TEXT_MAX_BYTES: usize = 51_200;
/// Hard upper bound on a single text slice regardless of requested `max_bytes`.
pub const TEXT_MAX_BYTES_CLAMP: usize = 204_800;
/// Maximum decoded size of a base64 blob before rendering is refused.
pub const BLOB_DECODE_CEILING_BYTES: usize = 50 * 1024 * 1024;
/// Total size bound for the spill tree; oldest files are evicted beyond it.
pub const SPILL_DIR_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Byte bound on server-supplied metadata strings (uri, mime type) copied into output.
pub const METADATA_MAX_BYTES: usize = 4096;

const PATTERN_CONTEXT_LINES: usize = 2;
const HUNK_SEPARATOR: &str = "--";

const MIME_EXTENSIONS: &[(&str, &str)] = &[
    ("application/gzip", "gz"),
    ("application/json", "json"),
    ("application/pdf", "pdf"),
    ("application/zip", "zip"),
    ("audio/mpeg", "mp3"),
    ("image/gif", "gif"),
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/webp", "webp"),
    ("text/csv", "csv"),
    ("video/mp4", "mp4"),
];

#[derive(Debug)]
pub enum RenderError {
    InvalidPattern { pattern: String, error: String },
    DecodedSizeExceeded,
    InvalidBase64(String),
    Io(io::Error),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPattern { pattern, error } => write!(
                f,
                "Invalid filter pattern '{pattern}': {error}. Provide a valid regex; \
                 lines matching it are returned with {PATTERN_CONTEXT_LINES} lines of context."
            ),
            Self::DecodedSizeExceeded => write!(
                f,
                "Decoded blob exceeds BLOB_DECODE_CEILING_BYTES ({} MiB); refusing to render it",
                BLOB_DECODE_CEILING_BYTES / (1024 * 1024)
            ),
            Self::InvalidBase64(error) => write!(f, "Invalid base64 in blob content: {error}"),
            Self::Io(error) => write!(f, "Failed to spill blob to disk: {error}"),
        }
    }
}

impl Error for RenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for RenderError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedText {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: usize,
    pub next_offset: Option<usize>,
}

#[derive(Debug)]
pub enum RenderedBlob {
    Text(String),
    Spilled(SpillMetadata),
}

#[derive(Debug, Serialize)]
pub struct SpillMetadata {
    pub spilled: bool,
    pub path: PathBuf,
    pub mime_type: Option<String>,
    pub sniffed: bool,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Pages `text` with UTF-8-boundary-safe slicing. When `pattern` is set, the
/// text is first reduced to matching lines plus context (grep-style, with
/// 1-based line-number prefixes), and all offset/size math operates on that
/// filtered stream.
pub fn render_text(
    text: &str,
    pattern: Option<&str>,
    offset: usize,
    max_bytes: Option<usize>,
) -> Result<RenderedText, RenderError> {
    let filtered = match pattern {
        Some(pattern) => Some(filter_lines(text, pattern)?),
        None => None,
    };
    let stream = filtered.as_deref().unwrap_or(text);
    let max_bytes = max_bytes
        .unwrap_or(DEFAULT_TEXT_MAX_BYTES)
        .min(TEXT_MAX_BYTES_CLAMP);
    let total_bytes = stream.len();
    let mut start = offset.min(total_bytes);
    while !stream.is_char_boundary(start) {
        start += 1;
    }
    let mut end = start.saturating_add(max_bytes).min(total_bytes);
    while !stream.is_char_boundary(end) {
        end -= 1;
    }
    // A max_bytes smaller than one codepoint would produce an empty page with
    // next_offset == offset, stalling paging; always advance by at least one.
    if end == start && start < total_bytes {
        end += 1;
        while !stream.is_char_boundary(end) {
            end += 1;
        }
    }

    let truncated = end < total_bytes;
    Ok(RenderedText {
        text: stream[start..end].to_string(),
        truncated,
        total_bytes,
        next_offset: truncated.then_some(end),
    })
}

/// Decodes a base64 blob, returning it as text when it is valid UTF-8 and
/// spilling it under `cache_dir()/mcp-resources/<server>/` otherwise.
pub fn render_blob(
    b64: &str,
    claimed_mime: Option<&str>,
    server: &str,
) -> Result<RenderedBlob, RenderError> {
    let spill_base = paths::cache_dir().join("mcp-resources");
    render_blob_at(b64, claimed_mime, server, &spill_base)
}

pub fn render_blob_at(
    b64: &str,
    claimed_mime: Option<&str>,
    server: &str,
    spill_base: &Path,
) -> Result<RenderedBlob, RenderError> {
    let decoded = decode_base64_bounded(b64)?;
    let decoded = match String::from_utf8(decoded) {
        Ok(text) => return Ok(RenderedBlob::Text(text)),
        Err(error) => error.into_bytes(),
    };
    let sha256 = format!("{:x}", Sha256::digest(&decoded));
    let dir = spill_base.join(sanitize_server(server));
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{sha256}.{}", extension_for_mime(claimed_mime)));

    // Writes land in a temp file and are renamed into place, so a visible
    // file at the final path is always complete and the dedup check below is
    // race-safe across processes (same sha means same content).
    if !path.exists() {
        static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let temp = dir.join(format!(
            "{sha256}.tmp-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let written = options
            .open(&temp)
            .and_then(|mut file| file.write_all(&decoded))
            .and_then(|()| fs::rename(&temp, &path));
        if let Err(error) = written {
            let _ = fs::remove_file(&temp);
            return Err(RenderError::Io(error));
        }
    }
    enforce_spill_bound(spill_base, SPILL_DIR_MAX_BYTES, &path);

    Ok(RenderedBlob::Spilled(SpillMetadata {
        spilled: true,
        path,
        mime_type: claimed_mime.map(str::to_string),
        sniffed: false,
        size_bytes: decoded.len() as u64,
        sha256,
    }))
}

/// Truncates `text` to at most `max_bytes`, rounding the cut point back to a
/// UTF-8 character boundary.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Bounds a server-supplied metadata string to [`METADATA_MAX_BYTES`],
/// appending a marker citing the constant when the input is truncated.
pub fn clamp_metadata(text: &str) -> String {
    if text.len() <= METADATA_MAX_BYTES {
        return text.to_string();
    }

    let clamped = truncate_utf8(text, METADATA_MAX_BYTES);
    format!("{clamped} [truncated: exceeds METADATA_MAX_BYTES ({METADATA_MAX_BYTES} bytes)]")
}

fn filter_lines(text: &str, pattern: &str) -> Result<String, RenderError> {
    let regex = Regex::new(pattern).map_err(|error| RenderError::InvalidPattern {
        pattern: pattern.to_string(),
        error: error.to_string(),
    })?;
    let lines: Vec<&str> = text.lines().collect();
    // fancy_regex can also fail at match time (backtracking limits); treat
    // that as a non-match rather than failing the whole render.
    let is_match: Vec<bool> = lines
        .iter()
        .map(|line| regex.is_match(line).unwrap_or(false))
        .collect();

    let mut keep = vec![false; lines.len()];
    for (i, _) in is_match.iter().enumerate().filter(|&(_, matched)| *matched) {
        let start = i.saturating_sub(PATTERN_CONTEXT_LINES);
        let end = (i + PATTERN_CONTEXT_LINES).min(lines.len() - 1);
        keep[start..=end].fill(true);
    }

    let mut out: Vec<String> = Vec::new();
    let mut prev_kept: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        if prev_kept.is_some_and(|prev| i > prev + 1) {
            out.push(HUNK_SEPARATOR.to_string());
        }
        let marker = if is_match[i] { ':' } else { '-' };
        out.push(format!("{}{marker}{line}", i + 1));
        prev_kept = Some(i);
    }

    Ok(out.join("\n"))
}

fn decode_base64_bounded(b64: &str) -> Result<Vec<u8>, RenderError> {
    // The encoded length puts a lower bound on the decoded size; reject
    // inputs that bound already proves oversized before decoding anything.
    let min_decoded = (b64.len() / 4).saturating_mul(3).saturating_sub(2);
    if min_decoded > BLOB_DECODE_CEILING_BYTES {
        return Err(RenderError::DecodedSizeExceeded);
    }

    let mut reader = DecoderReader::new(b64.as_bytes(), &STANDARD);
    let mut decoded = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(decoded),
            Ok(n) => {
                if decoded.len() + n > BLOB_DECODE_CEILING_BYTES {
                    return Err(RenderError::DecodedSizeExceeded);
                }
                decoded.extend_from_slice(&chunk[..n]);
            }
            Err(error) => return Err(RenderError::InvalidBase64(error.to_string())),
        }
    }
}

/// Maps a server-controlled mime type to a spill-file extension via an exact
/// allowlist lookup; anything unrecognized falls back to `bin`.
fn extension_for_mime(mime: Option<&str>) -> &'static str {
    let Some(mime) = mime else {
        return "bin";
    };
    let bare = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let ext = MIME_EXTENSIONS
        .iter()
        .find(|(known, _)| *known == bare)
        .map(|(_, ext)| *ext)
        .unwrap_or("bin");
    let safe = !ext.is_empty()
        && ext.len() <= 8
        && ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());

    if safe { ext } else { "bin" }
}

fn sanitize_server(server: &str) -> String {
    let mut sanitized: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    // Windows strips trailing dots at create time, which would make the
    // constructed path disagree with the on-disk name.
    while sanitized.ends_with('.') {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        return "_".to_string();
    }
    // Windows reserves device names (bare or with any extension).
    let stem = sanitized.split('.').next().unwrap_or("");
    if is_windows_reserved(stem) {
        sanitized.insert(0, '_');
    }
    sanitized
}

fn is_windows_reserved(stem: &str) -> bool {
    let lower = stem.to_ascii_lowercase();
    matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || (lower.len() == 4
            && (lower.starts_with("com") || lower.starts_with("lpt"))
            && matches!(lower.as_bytes()[3], b'1'..=b'9'))
}

struct SpillEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn enforce_spill_bound(base: &Path, max_total: u64, protect: &Path) {
    let mut entries = Vec::new();
    collect_spill_files(base, &mut entries);
    evict_oldest(entries, max_total, protect);
}

/// Best-effort eviction: the spill dir is shared across processes, so a file
/// vanishing underneath us (`NotFound`) is expected and never fails the spill.
fn evict_oldest(mut entries: Vec<SpillEntry>, max_total: u64, protect: &Path) {
    let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
    if total <= max_total {
        return;
    }
    entries.sort_by_key(|entry| entry.modified);
    for entry in &entries {
        if total <= max_total {
            break;
        }

        // Filenames are content-hashed, so name equality is sufficient and
        // survives filesystems that normalize directory names (case folding,
        // trailing-dot stripping) where a full-path comparison would miss.
        if entry.path.file_name() == protect.file_name() {
            continue;
        }

        match fs::remove_file(&entry.path) {
            Ok(()) => total -= entry.size,
            Err(error) if error.kind() == ErrorKind::NotFound => total -= entry.size,
            Err(_) => {}
        }
    }
}

fn collect_spill_files(dir: &Path, out: &mut Vec<SpillEntry>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_spill_files(&path, out);
        } else if metadata.is_file() {
            out.push(SpillEntry {
                path,
                size: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::env;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn with_spill_base<F: FnOnce(&Path)>(f: F) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "{}-{}",
            process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let base = env::temp_dir().join(format!("coyote-render-test-{unique}"));
        fs::create_dir_all(&base).unwrap();
        f(&base);
        let _ = fs::remove_dir_all(&base);
    }

    fn set_mtime(path: &Path, secs_after_epoch: u64) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs_after_epoch))
            .unwrap();
    }

    fn write_spill_file(dir: &Path, name: &str, len: usize, mtime_secs: u64) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, vec![0u8; len]).unwrap();
        set_mtime(&path, mtime_secs);
        path
    }

    const TEN_LINES: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten";

    #[test]
    fn slices_basic_ascii_page() {
        let rendered = render_text("hello world", None, 0, Some(5)).unwrap();

        assert_eq!(rendered.text, "hello");
        assert!(rendered.truncated);
        assert_eq!(rendered.total_bytes, 11);
        assert_eq!(rendered.next_offset, Some(5));
    }

    #[test]
    fn offset_mid_codepoint_rounds_forward() {
        // 'é' occupies bytes 1..3; offset 2 lands inside it.
        let rendered = render_text("héllo", None, 2, None).unwrap();

        assert_eq!(rendered.text, "llo");
        assert!(!rendered.truncated);
        assert_eq!(rendered.next_offset, None);
    }

    #[test]
    fn end_mid_codepoint_rounds_backward() {
        // 'é' occupies bytes 1..3; offset 0 + max_bytes 2 lands inside it.
        let rendered = render_text("aé", None, 0, Some(2)).unwrap();

        assert_eq!(rendered.text, "a");
        assert!(rendered.truncated);
        assert_eq!(rendered.total_bytes, 3);
        assert_eq!(rendered.next_offset, Some(1));

        let rest = render_text("aé", None, 1, Some(2)).unwrap();

        assert_eq!(rest.text, "é");
        assert!(!rest.truncated);
    }

    #[test]
    fn max_bytes_below_one_codepoint_still_advances() {
        // 'é' is 2 bytes; max_bytes 1 must not stall at next_offset == offset.
        let rendered = render_text("éa", None, 0, Some(1)).unwrap();

        assert_eq!(rendered.text, "é");
        assert!(rendered.truncated);
        assert_eq!(rendered.total_bytes, 3);
        assert_eq!(rendered.next_offset, Some(2));
    }

    #[test]
    fn offset_past_eof_returns_empty() {
        let rendered = render_text("short", None, 100, None).unwrap();

        assert_eq!(rendered.text, "");
        assert!(!rendered.truncated);
        assert_eq!(rendered.total_bytes, 5);
        assert_eq!(rendered.next_offset, None);
    }

    #[test]
    fn exact_fit_is_not_truncated() {
        let rendered = render_text("exact", None, 0, Some(5)).unwrap();

        assert_eq!(rendered.text, "exact");
        assert!(!rendered.truncated);
        assert_eq!(rendered.next_offset, None);
    }

    #[test]
    fn default_max_bytes_is_default_text_max_bytes() {
        let text = "a".repeat(DEFAULT_TEXT_MAX_BYTES + 1);

        let rendered = render_text(&text, None, 0, None).unwrap();

        assert_eq!(rendered.text.len(), DEFAULT_TEXT_MAX_BYTES);
        assert!(rendered.truncated);
        assert_eq!(rendered.next_offset, Some(DEFAULT_TEXT_MAX_BYTES));
    }

    #[test]
    fn max_bytes_above_clamp_is_clamped() {
        let text = "a".repeat(TEXT_MAX_BYTES_CLAMP + 1);

        let rendered = render_text(&text, None, 0, Some(usize::MAX)).unwrap();

        assert_eq!(rendered.text.len(), TEXT_MAX_BYTES_CLAMP);
        assert!(rendered.truncated);
        assert_eq!(rendered.next_offset, Some(TEXT_MAX_BYTES_CLAMP));
    }

    #[test]
    fn truncate_utf8_rounds_back_to_char_boundary() {
        // 'é' occupies bytes 1..3; a cut at byte 2 lands inside it.
        assert_eq!(truncate_utf8("aé", 2), "a");
        assert_eq!(truncate_utf8("aé", 3), "aé");
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("abc", 0), "");
    }

    #[test]
    fn clamp_metadata_appends_marker_only_when_oversized() {
        assert_eq!(clamp_metadata("text/plain"), "text/plain");

        let long = "u".repeat(METADATA_MAX_BYTES + 1);

        let clamped = clamp_metadata(&long);

        assert!(clamped.starts_with(&"u".repeat(METADATA_MAX_BYTES)));
        assert!(clamped.contains("METADATA_MAX_BYTES"));
        assert!(clamped.contains(&METADATA_MAX_BYTES.to_string()));
    }

    #[test]
    fn pattern_emits_matches_with_context_and_line_numbers() {
        let rendered = render_text(TEN_LINES, Some("^five$"), 0, None).unwrap();

        assert_eq!(rendered.text, "3-three\n4-four\n5:five\n6-six\n7-seven");
        assert!(!rendered.truncated);
        assert_eq!(rendered.total_bytes, rendered.text.len());
    }

    #[test]
    fn pattern_separates_disjoint_hunks() {
        let rendered = render_text(TEN_LINES, Some("^(two|nine)$"), 0, None).unwrap();

        assert_eq!(
            rendered.text,
            "1-one\n2:two\n3-three\n4-four\n--\n7-seven\n8-eight\n9:nine\n10-ten"
        );
    }

    #[test]
    fn pattern_merges_adjacent_hunks_without_duplicates() {
        let rendered = render_text(TEN_LINES, Some("^(two|six)$"), 0, None).unwrap();

        assert_eq!(
            rendered.text,
            "1-one\n2:two\n3-three\n4-four\n5-five\n6:six\n7-seven\n8-eight"
        );
        assert!(!rendered.text.contains(HUNK_SEPARATOR));
    }

    #[test]
    fn pattern_paging_walks_the_filtered_stream() {
        let full = render_text(TEN_LINES, Some("^t"), 0, None).unwrap();
        assert!(!full.truncated);

        let mut assembled = String::new();
        let mut offset = 0;
        loop {
            let page = render_text(TEN_LINES, Some("^t"), offset, Some(7)).unwrap();
            assert_eq!(page.total_bytes, full.text.len());
            assembled.push_str(&page.text);
            match page.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }

        assert_eq!(assembled, full.text);
    }

    #[test]
    fn pattern_with_no_matches_returns_empty() {
        let rendered = render_text(TEN_LINES, Some("^zebra$"), 0, None).unwrap();

        assert_eq!(rendered.text, "");
        assert_eq!(rendered.total_bytes, 0);
        assert!(!rendered.truncated);
        assert_eq!(rendered.next_offset, None);
    }

    #[test]
    fn invalid_pattern_is_a_teaching_error() {
        let parse_error = Regex::new("(").unwrap_err().to_string();

        let err = render_text("text", Some("("), 0, None).unwrap_err();

        assert!(matches!(err, RenderError::InvalidPattern { .. }));
        let message = err.to_string();
        assert!(message.contains("'('"));
        assert!(message.contains(&parse_error));
    }

    #[test]
    fn utf8_blob_decodes_to_text_without_spilling() {
        with_spill_base(|base| {
            let b64 = STANDARD.encode("hello ✓ world");

            let rendered = render_blob_at(&b64, Some("text/plain"), "srv", base).unwrap();

            let RenderedBlob::Text(text) = rendered else {
                panic!("expected text variant");
            };
            assert_eq!(text, "hello ✓ world");
            assert_eq!(fs::read_dir(base).unwrap().count(), 0);
        });
    }

    #[test]
    fn binary_blob_spills_with_metadata_and_0600_perms() {
        with_spill_base(|base| {
            let data: &[u8] = &[0xff, 0xfe, 0x00, 0x88, 0x01];
            let b64 = STANDARD.encode(data);

            let rendered = render_blob_at(&b64, Some("application/pdf"), "docs", base).unwrap();

            let RenderedBlob::Spilled(meta) = rendered else {
                panic!("expected spilled variant");
            };
            let expected_sha = format!("{:x}", Sha256::digest(data));
            assert_eq!(meta.sha256, expected_sha);
            assert_eq!(
                meta.path,
                base.join("docs").join(format!("{expected_sha}.pdf"))
            );
            assert_eq!(meta.size_bytes, data.len() as u64);
            assert_eq!(meta.mime_type.as_deref(), Some("application/pdf"));
            assert!(!meta.sniffed);
            assert!(meta.spilled);
            assert_eq!(fs::read(&meta.path).unwrap(), data);
            #[cfg(unix)]
            {
                let mode = fs::metadata(&meta.path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        });
    }

    #[test]
    fn decode_ceiling_rejects_oversized_blob() {
        with_spill_base(|base| {
            // base64 of 51 MiB of zero bytes is just a repeated-'A' string.
            let encoded = "A".repeat(51 * 1024 * 1024 / 3 * 4);

            let err = render_blob_at(&encoded, None, "srv", base).unwrap_err();

            assert!(matches!(err, RenderError::DecodedSizeExceeded));
            assert!(err.to_string().contains("BLOB_DECODE_CEILING_BYTES"));
        });
    }

    #[test]
    fn malformed_base64_is_rejected() {
        with_spill_base(|base| {
            let err = render_blob_at("!!!not base64!!!", None, "srv", base).unwrap_err();

            assert!(matches!(err, RenderError::InvalidBase64(_)));
        });
    }

    #[test]
    fn spill_dedup_returns_same_path_without_rewriting() {
        with_spill_base(|base| {
            let data: &[u8] = &[0xff, 0x01, 0x02];
            let b64 = STANDARD.encode(data);

            let RenderedBlob::Spilled(first) = render_blob_at(&b64, None, "srv", base).unwrap()
            else {
                panic!("expected spilled variant");
            };
            fs::write(&first.path, b"sentinel").unwrap();

            let RenderedBlob::Spilled(second) = render_blob_at(&b64, None, "srv", base).unwrap()
            else {
                panic!("expected spilled variant");
            };

            assert_eq!(second.path, first.path);
            assert_eq!(second.sha256, first.sha256);
            assert_eq!(fs::read(&second.path).unwrap(), b"sentinel");
        });
    }

    #[test]
    fn spill_metadata_serializes_spilled_true() {
        with_spill_base(|base| {
            let b64 = STANDARD.encode([0xffu8, 0x00]);

            let RenderedBlob::Spilled(meta) =
                render_blob_at(&b64, Some("image/png"), "srv", base).unwrap()
            else {
                panic!("expected spilled variant");
            };

            let value = serde_json::to_value(&meta).unwrap();
            assert_eq!(value["spilled"], serde_json::Value::Bool(true));
            assert_eq!(value["sniffed"], serde_json::Value::Bool(false));
            assert_eq!(value["sha256"].as_str(), Some(meta.sha256.as_str()));
            assert_eq!(value["mime_type"].as_str(), Some("image/png"));
        });
    }

    #[test]
    fn extension_allowlist_normalizes_and_defaults_to_bin() {
        assert_eq!(extension_for_mime(Some("application/pdf")), "pdf");
        assert_eq!(extension_for_mime(Some("image/png")), "png");
        assert_eq!(extension_for_mime(Some(" TEXT/CSV ; charset=utf-8")), "csv");
        assert_eq!(extension_for_mime(Some("../../evil")), "bin");
        assert_eq!(extension_for_mime(Some("image/png/../../x")), "bin");
        assert_eq!(extension_for_mime(Some("application/x-∞")), "bin");
        assert_eq!(extension_for_mime(Some("text/plain")), "bin");
        assert_eq!(extension_for_mime(None), "bin");
    }

    #[test]
    fn sanitize_server_strips_path_separators() {
        assert_eq!(sanitize_server("../evil/srv"), ".._evil_srv");
        assert_eq!(sanitize_server("srv name!"), "srv_name_");
        assert_eq!(sanitize_server(""), "_");
        assert_eq!(sanitize_server("."), "_");
        assert_eq!(sanitize_server(".."), "_");
        assert_eq!(sanitize_server("good-server_1.0"), "good-server_1.0");
    }

    #[test]
    fn sanitize_server_escapes_windows_reserved_names() {
        assert_eq!(sanitize_server("con"), "_con");
        assert_eq!(sanitize_server("CON"), "_CON");
        assert_eq!(sanitize_server("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_server("COM1"), "_COM1");
        assert_eq!(sanitize_server("lpt9"), "_lpt9");
        assert_eq!(sanitize_server("com0"), "com0");
        assert_eq!(sanitize_server("com10"), "com10");
        assert_eq!(sanitize_server("consul"), "consul");
    }

    #[test]
    fn sanitize_server_strips_trailing_dots_and_caps_length() {
        assert_eq!(sanitize_server("srv."), "srv");
        assert_eq!(sanitize_server("srv..."), "srv");
        assert_eq!(sanitize_server("..."), "_");
        let long = "a".repeat(100);
        assert_eq!(sanitize_server(&long).len(), 64);
    }

    #[test]
    fn spill_path_confines_crafted_server_and_mime() {
        with_spill_base(|base| {
            let b64 = STANDARD.encode([0xffu8, 0x00, 0x11]);

            let RenderedBlob::Spilled(meta) =
                render_blob_at(&b64, Some("../../evil"), "../evil/srv", base).unwrap()
            else {
                panic!("expected spilled variant");
            };

            assert!(meta.path.starts_with(base));
            let dir_name = meta.path.parent().unwrap().file_name().unwrap();
            assert_eq!(dir_name, ".._evil_srv");
            assert_eq!(meta.path.extension().unwrap(), "bin");
        });
    }

    #[test]
    fn eviction_removes_oldest_files_first_across_server_dirs() {
        with_spill_base(|base| {
            let srv_a = base.join("srv-a");
            let srv_b = base.join("srv-b");
            fs::create_dir_all(&srv_a).unwrap();
            fs::create_dir_all(&srv_b).unwrap();
            let oldest = write_spill_file(&srv_a, "a.bin", 100, 100);
            let middle = write_spill_file(&srv_b, "b.bin", 100, 200);
            let newest = write_spill_file(&srv_b, "c.bin", 100, 300);

            enforce_spill_bound(base, 150, &newest);

            assert!(!oldest.exists());
            assert!(!middle.exists());
            assert!(newest.exists());
        });
    }

    #[test]
    fn eviction_skips_protected_file() {
        with_spill_base(|base| {
            let srv = base.join("srv");
            fs::create_dir_all(&srv).unwrap();
            let oldest = write_spill_file(&srv, "a.bin", 100, 100);
            let middle = write_spill_file(&srv, "b.bin", 100, 200);
            let newest = write_spill_file(&srv, "c.bin", 100, 300);

            enforce_spill_bound(base, 250, &oldest);

            assert!(oldest.exists());
            assert!(!middle.exists());
            assert!(newest.exists());
        });
    }

    #[test]
    fn eviction_under_bound_is_noop() {
        with_spill_base(|base| {
            let srv = base.join("srv");
            fs::create_dir_all(&srv).unwrap();
            let first = write_spill_file(&srv, "a.bin", 100, 100);
            let second = write_spill_file(&srv, "b.bin", 100, 200);

            enforce_spill_bound(base, 1000, &second);

            assert!(first.exists());
            assert!(second.exists());
        });
    }

    #[test]
    fn eviction_tolerates_already_removed_entries() {
        with_spill_base(|base| {
            let srv = base.join("srv");
            fs::create_dir_all(&srv).unwrap();
            let real = write_spill_file(&srv, "real.bin", 100, 200);
            let entries = vec![
                SpillEntry {
                    path: srv.join("ghost.bin"),
                    size: 100,
                    modified: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
                },
                SpillEntry {
                    path: real.clone(),
                    size: 100,
                    modified: SystemTime::UNIX_EPOCH + Duration::from_secs(200),
                },
            ];

            evict_oldest(entries, 50, &base.join("untouched"));

            assert!(!real.exists());
        });
    }
}
