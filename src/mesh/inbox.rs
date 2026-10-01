//! The staging inbox, where a peer's file lands before anyone looks at it. One layout,
//! `<cache_dir>/mesh/inbox/<instance_id>/<peer-dest8>/<rel>`, and one guard: the directory
//! a file will be written into must resolve inside the inbox root, checked before any
//! directory is created under it and again before the file is written, so a symlink
//! planted under the inbox cannot lead a write outside it. Nothing here logs: the path and
//! the bytes are the peer's. A file is written through a randomly named `.tmp-<uuid>`
//! sibling and renamed into place, so no name a peer chooses can alias the temp file.

use crate::mesh::wire_path::WirePath;
use crate::mesh::{hex_lower, mesh_cache_dir};

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

pub(crate) fn inbox_root(cache_dir: &Path, instance_id: &str) -> PathBuf {
    mesh_cache_dir(cache_dir).join("inbox").join(instance_id)
}

#[derive(Debug)]
pub(crate) enum StageError {
    /// The staging directory resolves outside the inbox root; nothing was written.
    Escaped,
    Io(io::Error),
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Escaped => write!(
                f,
                "The staging path resolves outside the inbox; nothing was written"
            ),
            Self::Io(err) => write!(f, "The file could not be staged: {}", err.kind()),
        }
    }
}

impl std::error::Error for StageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Escaped => None,
            Self::Io(err) => Some(err),
        }
    }
}

impl From<io::Error> for StageError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

pub(crate) struct InboxStaging {
    root: PathBuf,
}

impl InboxStaging {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn for_instance(cache_dir: &Path, instance_id: &str) -> Self {
        Self::new(inbox_root(cache_dir, instance_id))
    }

    /// Writes `bytes` under `<root>/<peer-dest8>/<rel>` and returns the absolute staged
    /// path. `peer_destination` is the peer's destination hash; its first eight characters,
    /// lower-cased, name the peer's directory. A file already at the target with the same
    /// `sha256` is reused without a write; one with a different hash keeps its place and
    /// the new bytes land beside it as `<stem>-<sha256[..8]><ext>`.
    pub(crate) fn stage(
        &self,
        peer_destination: &str,
        rel: &WirePath,
        sha256: &[u8; 32],
        bytes: &[u8],
    ) -> Result<PathBuf, StageError> {
        let dest8 = peer_dest8(peer_destination);
        fs::create_dir_all(self.root.join(&dest8))?;
        let canonical_root = dunce::canonicalize(&self.root)?;
        let target = canonical_root.join(&dest8).join(rel.to_relative_path());
        let Some(parent) = target.parent() else {
            return Err(StageError::Escaped);
        };
        let Some(existing) = parent.ancestors().find(|path| path.exists()) else {
            return Err(StageError::Escaped);
        };
        ensure_inside(&canonical_root, existing)?;
        fs::create_dir_all(parent)?;
        ensure_inside(&canonical_root, parent)?;

        let target = match existing_matches(&target, sha256)? {
            Some(true) => return Ok(target),
            None => target,
            Some(false) => {
                let suffixed = with_hash_suffix(&target, sha256);
                if existing_matches(&suffixed, sha256)? == Some(true) {
                    return Ok(suffixed);
                }
                suffixed
            }
        };
        write_staged(&target, bytes)?;
        Ok(target)
    }
}

fn peer_dest8(peer_destination: &str) -> String {
    peer_destination
        .chars()
        .take(8)
        .collect::<String>()
        .to_lowercase()
}

fn ensure_inside(canonical_root: &Path, path: &Path) -> Result<(), StageError> {
    if dunce::canonicalize(path)?.starts_with(canonical_root) {
        Ok(())
    } else {
        Err(StageError::Escaped)
    }
}

/// `None` when nothing is at `path`, otherwise whether what is there hashes to `sha256`.
fn existing_matches(path: &Path, sha256: &[u8; 32]) -> Result<Option<bool>, io::Error> {
    match fs::read(path) {
        Ok(existing) => Ok(Some(Sha256::digest(&existing).as_slice() == sha256)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn with_hash_suffix(target: &Path, sha256: &[u8; 32]) -> PathBuf {
    let mut name = target.file_stem().unwrap_or_default().to_os_string();
    name.push(format!("-{}", hex_lower(&sha256[..4])));
    if let Some(ext) = target.extension() {
        name.push(".");
        name.push(ext);
    }
    target.with_file_name(name)
}

/// Writes `bytes` to `.tmp-<uuid>` beside `target`, syncs and renames it into place; on
/// any failure the temp file is removed best-effort and the error returned.
fn write_staged(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let Some(parent) = target.parent() else {
        return Err(io::Error::other("the staging target has no parent"));
    };
    let tmp = parent.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let written = File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, target));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_support::TempDir;

    const PEER: &str = "ABCDEF0123456789abcdef0123456789";
    const DEST8: &str = "abcdef01";

    fn digest(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn staging(tmp: &TempDir) -> InboxStaging {
        InboxStaging::new(tmp.path.join("inbox"))
    }

    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(files_under(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    #[test]
    fn inbox_root_is_mesh_inbox_instance_under_cache_dir() {
        let cache_dir = Path::new("cache");
        assert_eq!(
            inbox_root(cache_dir, "inst"),
            cache_dir.join("mesh").join("inbox").join("inst")
        );
        assert_eq!(
            InboxStaging::for_instance(cache_dir, "inst").root,
            inbox_root(cache_dir, "inst")
        );
    }

    #[test]
    fn a_staged_file_lands_under_the_peer_directory_at_an_absolute_path() {
        let tmp = TempDir::new("inbox-stage");
        let inbox = staging(&tmp);
        let rel = WirePath::parse("docs/a.md").unwrap();

        let staged = inbox
            .stage(PEER, &rel, &digest(b"hello"), b"hello")
            .unwrap();

        let canonical_root = dunce::canonicalize(&inbox.root).unwrap();
        assert!(staged.is_absolute());
        assert!(staged.starts_with(&canonical_root), "{}", staged.display());
        assert_eq!(staged, canonical_root.join(DEST8).join("docs").join("a.md"));
        assert_eq!(fs::read(&staged).unwrap(), b"hello");
        assert_eq!(files_under(&inbox.root), [staged]);
    }

    #[test]
    fn identical_content_staged_twice_reuses_the_same_path() {
        let tmp = TempDir::new("inbox-same");
        let inbox = staging(&tmp);
        let rel = WirePath::parse("docs/a.md").unwrap();
        let sha = digest(b"hello");

        let first = inbox.stage(PEER, &rel, &sha, b"hello").unwrap();
        let second = inbox.stage(PEER, &rel, &sha, b"hello").unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read(&second).unwrap(), b"hello");
        assert_eq!(files_under(&inbox.root), [first]);
    }

    #[test]
    fn different_content_under_the_same_name_lands_beside_it_with_a_hash_suffix() {
        let tmp = TempDir::new("inbox-collide");
        let inbox = staging(&tmp);
        let rel = WirePath::parse("docs/a.md").unwrap();
        let other_sha = digest(b"other");

        let first = inbox
            .stage(PEER, &rel, &digest(b"hello"), b"hello")
            .unwrap();
        let second = inbox.stage(PEER, &rel, &other_sha, b"other").unwrap();
        let again = inbox.stage(PEER, &rel, &other_sha, b"other").unwrap();

        let suffix = hex_lower(&other_sha[..4]);
        assert_eq!(
            second,
            first.with_file_name(format!("a-{suffix}.md")),
            "{}",
            second.display()
        );
        assert_eq!(again, second);
        assert_eq!(fs::read(&first).unwrap(), b"hello");
        assert_eq!(fs::read(&second).unwrap(), b"other");

        let bare = WirePath::parse("docs/.bashrc").unwrap();
        let dotfile = inbox.stage(PEER, &bare, &digest(b"x"), b"x").unwrap();
        let clash = inbox.stage(PEER, &bare, &other_sha, b"other").unwrap();
        assert_eq!(clash, dotfile.with_file_name(format!(".bashrc-{suffix}")));
    }

    #[test]
    fn a_file_named_like_a_temp_file_survives_the_next_stage_beside_it() {
        let tmp = TempDir::new("inbox-tmp-name");
        let inbox = staging(&tmp);
        let dot_tmp = WirePath::parse("x.tmp").unwrap();
        let bare = WirePath::parse("x").unwrap();

        let first = inbox
            .stage(PEER, &dot_tmp, &digest(b"first"), b"first")
            .unwrap();
        let second = inbox
            .stage(PEER, &bare, &digest(b"second"), b"second")
            .unwrap();

        assert_eq!(fs::read(&first).unwrap(), b"first");
        assert_eq!(fs::read(&second).unwrap(), b"second");
        let mut found = files_under(&inbox.root);
        found.sort();
        assert_eq!(found, [second, first]);
    }

    #[test]
    fn a_stage_leaves_no_temp_file_behind() {
        let tmp = TempDir::new("inbox-no-temp");
        let inbox = staging(&tmp);
        let rel = WirePath::parse("docs/a.md").unwrap();

        inbox.stage(PEER, &rel, &digest(b"x"), b"x").unwrap();

        let found = files_under(&inbox.root);
        assert_eq!(found.len(), 1);
        assert!(
            found.iter().all(|path| !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".tmp-"))),
            "{found:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_leading_outside_the_root_is_refused_before_any_write() {
        let tmp = TempDir::new("inbox-symlink");
        let inbox = staging(&tmp);
        let outside = tmp.path.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let peer_dir = inbox.root.join(DEST8);
        fs::create_dir_all(&peer_dir).unwrap();
        std::os::unix::fs::symlink(&outside, peer_dir.join("link")).unwrap();

        for text in ["link/a.md", "link/deep/a.md"] {
            let rel = WirePath::parse(text).unwrap();
            let err = inbox.stage(PEER, &rel, &digest(b"x"), b"x").unwrap_err();
            assert!(matches!(err, StageError::Escaped), "{text}: {err}");
            assert!(
                !err.to_string().contains(tmp.path.to_str().unwrap()),
                "{err}"
            );
        }
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(files_under(&inbox.root), Vec::<PathBuf>::new());
    }

    #[test]
    fn the_grammar_refuses_traversal_before_the_inbox_is_touched() {
        let tmp = TempDir::new("inbox-grammar");
        let inbox = staging(&tmp);
        fs::create_dir_all(inbox.root.join(DEST8)).unwrap();

        for (text, rule) in [
            ("../../.bashrc", "segment"),
            ("C:\\x", "backslash"),
            ("C:x", "drive_letter"),
            ("a\0b", "control"),
            ("docs/he\u{301}llo.md", "nfc"),
        ] {
            assert_eq!(WirePath::parse(text).unwrap_err().rule, rule, "{text:?}");
            assert_eq!(files_under(&inbox.root), Vec::<PathBuf>::new());
        }
        assert_eq!(
            fs::read_dir(&inbox.root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [DEST8]
        );
    }

    #[test]
    fn the_peer_directory_is_the_lower_cased_first_eight_characters() {
        assert_eq!(peer_dest8(PEER), DEST8);
        assert_eq!(peer_dest8("AbC"), "abc");
    }
}
