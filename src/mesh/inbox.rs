//! The staging inbox, where a peer's file lands before anyone looks at it. One layout,
//! `<cache_dir>/mesh/inbox/<instance_id>/<peer-dest8>/<rel>`, and one guard: the directory
//! a file will be written into must resolve inside the inbox root, checked before any
//! directory is created under it and again before the file is written, so a symlink
//! planted under the inbox cannot lead a write outside it. Nothing here logs: the path and
//! the bytes are the peer's. A file is written through a randomly named `.tmp-<uuid>`
//! sibling and hard-linked into place, so no name a peer chooses can alias the temp file
//! and nothing already at the target is ever overwritten.

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
    /// The target and its hash-suffixed sibling both hold other bytes; nothing was written.
    Collision,
    Io(io::Error),
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Escaped => write!(
                f,
                "The staging path resolves outside the inbox; nothing was written"
            ),
            Self::Collision => write!(
                f,
                "The staging path and its hash-suffixed sibling already hold other content; nothing was written"
            ),
            Self::Io(err) => write!(f, "The file could not be staged: {}", err.kind()),
        }
    }
}

impl std::error::Error for StageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Escaped | Self::Collision => None,
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
    /// the new bytes land beside it as `<stem>-<sha256[..8]><ext>`. When that name too
    /// holds other bytes, or a file appears at the target between the check and the
    /// write, nothing is overwritten: `Collision`. A root that is gone again by the time
    /// its ancestors are walked is `Io(NotFound)`, not `Escaped`.
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
        let relative = rel.to_relative_path();
        let peer_dir = canonical_root.join(&dest8);
        let parent = peer_dir.join(relative.parent().unwrap_or(Path::new("")));
        let target = peer_dir.join(&relative);
        let Some(existing) = parent
            .ancestors()
            .take_while(|path| path.starts_with(&canonical_root))
            .find(|path| path.exists())
        else {
            return Err(StageError::Io(io::Error::from(io::ErrorKind::NotFound)));
        };
        ensure_inside(&canonical_root, existing)?;
        fs::create_dir_all(&parent)?;
        ensure_inside(&canonical_root, &parent)?;

        let target = match existing_matches(&target, sha256)? {
            Some(true) => return Ok(target),
            None => target,
            Some(false) => {
                let suffixed = with_hash_suffix(&target, sha256);
                match existing_matches(&suffixed, sha256)? {
                    Some(true) => return Ok(suffixed),
                    Some(false) => return Err(StageError::Collision),
                    None => suffixed,
                }
            }
        };
        match write_staged(&target, bytes) {
            Ok(()) => Ok(target),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                match existing_matches(&target, sha256)? {
                    Some(true) => Ok(target),
                    _ => Err(StageError::Collision),
                }
            }
            Err(err) => Err(err.into()),
        }
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

/// Writes `bytes` to `.tmp-<uuid>` beside `target`, syncs and hard-links it into place,
/// which fails with `AlreadyExists` rather than replacing a file already at `target`. The
/// temp name is removed either way, best-effort.
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
        .and_then(|()| fs::hard_link(&tmp, target));
    let _ = fs::remove_file(&tmp);
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

    /// Three contents meeting at one name: `first` holds `<name>`, `third` was staged
    /// under the `<stem>-<hash8><ext>` that `second` would take, so `second` finds both
    /// names holding other bytes and is refused with neither file touched.
    #[test]
    fn a_third_content_whose_suffixed_name_is_also_taken_is_a_collision_not_an_overwrite() {
        let tmp = TempDir::new("inbox-collision");
        let inbox = staging(&tmp);
        let rel = WirePath::parse("docs/a.md").unwrap();
        let second_sha = digest(b"second");
        let suffixed =
            WirePath::parse(&format!("docs/a-{}.md", hex_lower(&second_sha[..4]))).unwrap();

        let first = inbox
            .stage(PEER, &rel, &digest(b"first"), b"first")
            .unwrap();
        let third = inbox
            .stage(PEER, &suffixed, &digest(b"third"), b"third")
            .unwrap();
        assert_eq!(
            third,
            first.with_file_name(suffixed.to_relative_path().file_name().unwrap())
        );

        let err = inbox.stage(PEER, &rel, &second_sha, b"second").unwrap_err();

        assert!(matches!(err, StageError::Collision), "{err}");
        assert!(!err.to_string().contains("docs"), "{err}");
        assert_eq!(fs::read(&first).unwrap(), b"first");
        assert_eq!(fs::read(&third).unwrap(), b"third");
        let mut found = files_under(&inbox.root);
        found.sort();
        assert_eq!(found, [third, first]);
    }

    /// The target and its hash-suffixed sibling were put there by something other than
    /// a stage; the refusal is the same and neither is touched.
    #[test]
    fn a_pre_planted_target_and_sibling_holding_other_bytes_are_a_collision() {
        let tmp = TempDir::new("inbox-planted-collision");
        let inbox = staging(&tmp);
        let peer_dir = inbox.root.join(DEST8);
        fs::create_dir_all(peer_dir.join("docs")).unwrap();
        let sha = digest(b"third");
        let target = peer_dir.join("docs").join("a.md");
        let suffixed = peer_dir
            .join("docs")
            .join(format!("a-{}.md", hex_lower(&sha[..4])));
        fs::write(&target, b"first").unwrap();
        fs::write(&suffixed, b"second").unwrap();

        let err = inbox
            .stage(PEER, &WirePath::parse("docs/a.md").unwrap(), &sha, b"third")
            .unwrap_err();

        assert!(matches!(err, StageError::Collision), "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"first");
        assert_eq!(fs::read(&suffixed).unwrap(), b"second");
        let mut found = files_under(&inbox.root);
        found.sort();
        let mut planted = vec![target, suffixed];
        planted.sort();
        assert_eq!(found, planted);
    }

    #[test]
    fn a_pre_planted_target_holding_the_same_bytes_is_reused_without_a_write() {
        let tmp = TempDir::new("inbox-planted-reuse");
        let inbox = staging(&tmp);
        let peer_dir = inbox.root.join(DEST8);
        fs::create_dir_all(peer_dir.join("docs")).unwrap();
        let target = peer_dir.join("docs").join("a.md");
        fs::write(&target, b"same").unwrap();

        let staged = inbox
            .stage(
                PEER,
                &WirePath::parse("docs/a.md").unwrap(),
                &digest(b"same"),
                b"same",
            )
            .unwrap();

        assert_eq!(staged, dunce::canonicalize(&target).unwrap());
        assert_eq!(fs::read(&staged).unwrap(), b"same");
        assert_eq!(files_under(&inbox.root), [target]);
    }

    /// A root that is not a directory cannot hold a peer directory; that is an I/O
    /// failure of the inbox, not a path leading outside it.
    #[test]
    fn a_root_that_is_a_file_is_an_io_error_not_an_escape() {
        let tmp = TempDir::new("inbox-root-file");
        let inbox = staging(&tmp);
        fs::write(&inbox.root, b"not a directory").unwrap();

        let err = inbox
            .stage(
                PEER,
                &WirePath::parse("docs/a.md").unwrap(),
                &digest(b"x"),
                b"x",
            )
            .unwrap_err();

        assert!(matches!(err, StageError::Io(_)), "{err}");
        assert_eq!(fs::read(&inbox.root).unwrap(), b"not a directory");
    }

    #[test]
    fn the_publish_step_never_replaces_a_file_already_at_the_target() {
        let tmp = TempDir::new("inbox-no-clobber");
        let target = tmp.path.join("a.md");
        fs::write(&target, b"already here").unwrap();

        let err = write_staged(&target, b"new").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"already here");
        assert_eq!(files_under(&tmp.path), [target]);
    }

    /// The inbox is reopened per message; nothing on the way in may clear what an earlier
    /// message staged.
    #[test]
    fn reopening_the_inbox_for_the_instance_keeps_what_was_staged_before() {
        let tmp = TempDir::new("inbox-restage");
        let first = InboxStaging::for_instance(&tmp.path, "inst")
            .stage(
                PEER,
                &WirePath::parse("docs/a.md").unwrap(),
                &digest(b"first"),
                b"first",
            )
            .unwrap();

        let second = InboxStaging::for_instance(&tmp.path, "inst")
            .stage(
                PEER,
                &WirePath::parse("docs/b.md").unwrap(),
                &digest(b"second"),
                b"second",
            )
            .unwrap();

        assert_eq!(fs::read(&first).unwrap(), b"first");
        assert_eq!(fs::read(&second).unwrap(), b"second");
        let mut found = files_under(&inbox_root(&tmp.path, "inst"));
        found.sort();
        assert_eq!(found, [first, second]);
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

    /// Usage probe, criterion (b) "canonicalise the inbox root": an operator whose cache
    /// directory is itself a symlink (a relocated `~/.cache`) stages normally. The staged
    /// path is absolute, resolved through the link and under the real root, the file holds
    /// the bytes, and the same file reached through the link and through the real path is
    /// one file — a second stage of the same bytes either way reuses it.
    #[cfg(unix)]
    #[test]
    fn usage_probe_a_symlinked_inbox_root_stages_normally_under_the_resolved_root() {
        let tmp = TempDir::new("inbox-symlinked-root");
        let real_cache = tmp.path.join("real-cache");
        fs::create_dir_all(&real_cache).unwrap();
        let linked_cache = tmp.path.join("cache");
        std::os::unix::fs::symlink(&real_cache, &linked_cache).unwrap();

        let through_link = InboxStaging::for_instance(&linked_cache, "inst");
        let rel = WirePath::parse("docs/a.md").unwrap();
        let staged = through_link
            .stage(PEER, &rel, &digest(b"x"), b"x")
            .expect("a symlinked root is a valid root");

        assert!(staged.is_absolute());
        assert_eq!(fs::read(&staged).unwrap(), b"x");
        let resolved_root = dunce::canonicalize(inbox_root(&real_cache, "inst")).unwrap();
        assert_eq!(
            staged,
            resolved_root.join(DEST8).join("docs").join("a.md"),
            "resolved through the link, under the real root"
        );
        assert!(
            !staged.starts_with(&linked_cache),
            "the returned path does not go through the link: {staged:?}"
        );
        assert_eq!(files_under(&resolved_root), vec![staged.clone()]);

        let through_real = InboxStaging::for_instance(&real_cache, "inst");
        assert_eq!(
            through_real.stage(PEER, &rel, &digest(b"x"), b"x").unwrap(),
            staged,
            "the same bytes under the same name are one file whichever way the root is named"
        );
        assert_eq!(
            through_link.stage(PEER, &rel, &digest(b"x"), b"x").unwrap(),
            staged
        );
        assert_eq!(files_under(&resolved_root), vec![staged]);
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
            ("con .txt", "reserved_name"),
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

    /// Usage probe, criterion (b): a peer's two names may clash as a file and a directory
    /// (`docs` after `docs/a.md`, or `a.md/b.md` after `a.md`). Neither direction may
    /// replace or remove what is already staged; the later stage is refused with an error
    /// that names no path, and the inbox holds exactly the files it held before.
    #[test]
    fn a_name_clashing_with_a_staged_directory_or_file_is_refused_without_touching_either() {
        let tmp = TempDir::new("inbox-file-dir-clash");
        let inbox = staging(&tmp);

        let nested = inbox
            .stage(
                PEER,
                &WirePath::parse("docs/a.md").unwrap(),
                &digest(b"nested"),
                b"nested",
            )
            .unwrap();
        let err = inbox
            .stage(
                PEER,
                &WirePath::parse("docs").unwrap(),
                &digest(b"flat"),
                b"flat",
            )
            .unwrap_err();
        assert!(!matches!(err, StageError::Escaped), "{err}");
        assert!(!err.to_string().contains("docs"), "{err}");
        assert!(nested.parent().unwrap().is_dir());
        assert_eq!(fs::read(&nested).unwrap(), b"nested");

        let flat = inbox
            .stage(
                PEER,
                &WirePath::parse("b.md").unwrap(),
                &digest(b"flat"),
                b"flat",
            )
            .unwrap();
        let err = inbox
            .stage(
                PEER,
                &WirePath::parse("b.md/c.md").unwrap(),
                &digest(b"under"),
                b"under",
            )
            .unwrap_err();
        assert!(!matches!(err, StageError::Escaped), "{err}");
        assert!(flat.is_file());
        assert_eq!(fs::read(&flat).unwrap(), b"flat");

        let mut found = files_under(&inbox.root);
        found.sort();
        assert_eq!(found, [flat, nested]);
    }

    #[test]
    fn the_peer_directory_is_the_lower_cased_first_eight_characters() {
        assert_eq!(peer_dest8(PEER), DEST8);
        assert_eq!(peer_dest8("AbC"), "abc");
    }
}
