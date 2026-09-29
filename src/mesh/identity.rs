use crate::config::paths;
use crate::mesh::{mesh_config_dir, redact_hashes, rfc3339_utc, short};

use anyhow::{Context, Result, bail};
use lxmf_core::identity::PrivateIdentity;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Size of the persisted key: the x25519 secret followed by the ed25519 seed, 32 bytes each.
const PRIVATE_KEY_LENGTH: usize = 64;

/// Sibling of `identity.key`: one JSON line per rotation, oldest first.
pub(crate) const PREDECESSORS_FILE: &str = "identity.predecessors.jsonl";

/// Where this config dir keeps its mesh identity.
pub(crate) fn identity_path() -> PathBuf {
    mesh_config_dir(&paths::config_dir()).join("identity.key")
}

pub(crate) fn predecessors_path(identity_path: &Path) -> PathBuf {
    identity_path.with_file_name(PREDECESSORS_FILE)
}

/// The sibling `rotate_identity` writes the new key to before renaming it over the identity.
fn staged_path(identity_path: &Path) -> PathBuf {
    identity_path.with_added_extension("new")
}

/// One retired identity, as the predecessors file records it. Only the public hash is kept;
/// the private key it belonged to is gone. Unknown fields are tolerated so a line a newer
/// build wrote still reads on an older one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Predecessor {
    pub identity_hash: String,
    pub rotated_at: String,
    pub reason: String,
}

pub(crate) struct Rotation {
    pub old_fingerprint: String,
    pub new_fingerprint: String,
    /// How many predecessors the file records once this rotation's line is appended.
    pub predecessors: usize,
}

/// The identity's address hash as hex. This is what peers see; log lines still truncate it
/// with `short`, like every other hash.
pub(crate) fn fingerprint(identity: &PrivateIdentity) -> String {
    identity.address_hash().to_hex_string()
}

/// The identity at `path` as it stands; unlike `load_or_mint_identity` a missing key is an
/// error, since the caller wants to name or replace the one that exists. A staged key left
/// by an interrupted rotation is swept here too, so a dry run removes it as a start would.
pub(crate) fn current_identity(path: &Path) -> Result<PrivateIdentity> {
    remove_stale_staged_key(path)?;
    refuse_symlink(path)?;
    if fs::symlink_metadata(path).is_err() {
        bail!(
            "No mesh identity at '{}': `.mesh on` mints one the first time the node starts.",
            path.display()
        );
    }
    load_identity(path)
}

/// Rotation writes only regular files: renaming over a symlinked key would replace the link
/// and leave the old private key at its target, and appending through a symlinked history
/// would write wherever it points.
fn refuse_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "'{}' is a symlink; mesh identity rotation writes only regular files. Replace the link with the file it points to and try again.",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("Failed to read metadata of '{}'", path.display()))
        }
    }
}

/// Removes the key a rotation staged but never renamed into place, so the unused private
/// key it holds does not outlive the next start or rotation.
fn remove_stale_staged_key(path: &Path) -> Result<()> {
    let staged = staged_path(path);
    match fs::remove_file(&staged) {
        Ok(()) => {
            debug!(
                "Removed '{}' left by an interrupted mesh identity rotation",
                staged.display()
            );
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to remove '{}' left by an interrupted rotation",
                staged.display()
            )
        }),
    }
}

/// Replaces the identity at `path` with a freshly minted one and records the old hash in
/// the predecessors file. Only for a stopped node: the running node holds the old key in
/// memory and its announces, links and trust checks all name it, so re-keying live would
/// leave peers holding a destination that no longer answers. Once the node starts again it
/// announces a new destination hash under the new identity; the instance id is unchanged.
/// Peers that trusted the old identity or its destinations see a stranger and must trust
/// the new one deliberately, while this node's own trust list is unaffected.
///
/// `expected_old` is the fingerprint the caller showed the human; a key that no longer
/// matches it is left alone, since the consent was given for a different identity. The
/// predecessors file is read before anything is written so a corrupt history fails the
/// rotation with the key still in place.
///
/// The new key is written to a sibling file and renamed over `path`, so the old private key
/// is unlinked by the rename rather than overwritten or copied; the staged file is the only
/// other copy of the new key and is removed on any failure. The predecessors line is
/// appended after the rename: an interruption between the two loses the bookkeeping line,
/// never the key.
pub(crate) fn rotate_identity(
    path: &Path,
    expected_old: &str,
    now: SystemTime,
) -> Result<Rotation> {
    let old = current_identity(path)?;
    let old_fingerprint = fingerprint(&old);
    if old_fingerprint != expected_old {
        bail!(
            "The mesh identity changed since the dry run; run .mesh rotate again for a fresh token"
        );
    }
    refuse_symlink(&predecessors_path(path))?;
    let recorded = predecessors(path)?.len();
    let bytes = rand::random::<[u8; PRIVATE_KEY_LENGTH]>();
    let minted = PrivateIdentity::from_private_key_bytes(&bytes)
        .expect("64 random bytes are a valid identity");
    let new_fingerprint = fingerprint(&minted);
    let staged = staged_path(path);
    write_owner_only_file(&staged, &minted.to_private_key_bytes())?;
    if let Err(err) = fs::rename(&staged, path) {
        let mut context = format!(
            "Failed to replace '{}' with '{}'",
            path.display(),
            staged.display()
        );
        if let Err(cleanup) = fs::remove_file(&staged)
            && cleanup.kind() != ErrorKind::NotFound
        {
            warn!(
                "Failed to remove the staged mesh identity key after the rename failed: {}",
                redact_hashes(&cleanup.to_string())
            );
            context.push_str(&format!(
                "; the staged key at '{}' could not be removed",
                staged.display()
            ));
        }
        return Err(err).context(context);
    }
    info!(
        "Rotated mesh identity {} -> {}",
        short(&old_fingerprint),
        short(&new_fingerprint)
    );
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
    append_predecessor(
        &predecessors_path(path),
        &Predecessor {
            identity_hash: old_fingerprint.clone(),
            rotated_at: rfc3339_utc(now),
            reason: "rotate".to_string(),
        },
    )
    .with_context(|| {
        format!(
            "The mesh identity was rotated from {old_fingerprint} to {new_fingerprint}, but the old hash could not be recorded"
        )
    })?;
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
    load_identity(path).with_context(|| {
        format!(
            "The mesh identity was rotated from {old_fingerprint} to {new_fingerprint}, but the new key could not be read back"
        )
    })?;
    Ok(Rotation {
        old_fingerprint,
        new_fingerprint,
        predecessors: recorded + 1,
    })
}

/// Flushes `dir`'s entries so the rename and the new history file survive a crash; the
/// data is already on disk, so a failure here is worth a warning, not a failed rotation.
/// Directory handles cannot be opened for syncing off unix, so there it does nothing.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Err(err) = fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        warn!(
            "Failed to sync directory '{}': {}",
            dir.display(),
            redact_hashes(&err.to_string())
        );
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn append_predecessor(path: &Path, predecessor: &Predecessor) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("Failed to open '{}'", path.display()))?;
    let mut line = serde_json::to_string(predecessor)?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("Failed to append to '{}'", path.display()))
}

/// Every identity `identity_path` has retired, oldest first; none when the file is absent.
/// A line that does not parse fails the whole read so a truncated or edited file is never
/// shown as a shorter history.
pub(crate) fn predecessors(identity_path: &Path) -> Result<Vec<Predecessor>> {
    let path = predecessors_path(identity_path);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to read '{}'", path.display()));
        }
    };
    let mut out = Vec::new();
    for (index, line) in io::BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("Failed to read '{}'", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let predecessor: Predecessor = serde_json::from_str(&line).with_context(|| {
            format!(
                "Mesh identity predecessors file '{}' is corrupt at line {}. Fix or remove that line; the file records only retired public hashes, so nothing is lost by editing it.",
                path.display(),
                index + 1
            )
        })?;
        out.push(predecessor);
    }
    Ok(out)
}

/// Loads the identity at `path`, minting and persisting a fresh one if none exists yet.
/// A key that is the wrong length or, on unix, readable by other users is refused rather
/// than used silently.
pub(crate) fn load_or_mint_identity(path: &Path) -> Result<PrivateIdentity> {
    remove_stale_staged_key(path)?;
    if path.exists() {
        return load_identity(path);
    }

    let bytes = rand::random::<[u8; PRIVATE_KEY_LENGTH]>();
    let identity = PrivateIdentity::from_private_key_bytes(&bytes)
        .expect("64 random bytes are a valid identity");
    if let Some(parent) = path.parent() {
        let mut dir = fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir.mode(0o700);
        }
        dir.create(parent)
            .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
    }
    match write_owner_only_file(path, &identity.to_private_key_bytes()) {
        Ok(()) => {
            debug!("Minted mesh identity {}", short(&fingerprint(&identity)));
            Ok(identity)
        }
        // Another process minted first; its key is the one every later start will load.
        Err(err) if is_already_exists(&err) => load_identity(path),
        Err(err) => Err(err),
    }
}

fn load_identity(path: &Path) -> Result<PrivateIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(path)
            .with_context(|| format!("Failed to read metadata of '{}'", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "Mesh identity file '{}' is readable by other users (mode {:o}). A private key other users can read must not be used. Run `chmod 600 {}` and try again.",
                path.display(),
                mode & 0o777,
                path.display()
            );
        }
    }

    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read mesh identity file '{}'", path.display()))?;
    if bytes.len() != PRIVATE_KEY_LENGTH {
        bail!(
            "Mesh identity file '{}' is corrupt: expected {PRIVATE_KEY_LENGTH} bytes, found {}. Move the file aside to mint a new identity; any trust other peers hold for the old identity is lost.",
            path.display(),
            bytes.len()
        );
    }
    PrivateIdentity::from_private_key_bytes(&bytes)
        .with_context(|| format!("Failed to load mesh identity from '{}'", path.display()))
}

fn is_already_exists(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|io| io.kind() == ErrorKind::AlreadyExists)
}

/// Creates `path` with `bytes` so that only the owner can read it at any point in its life.
/// The file must not already exist; a concurrent creator surfaces as `ErrorKind::AlreadyExists`.
pub(crate) fn write_owner_only_file(
    #[cfg_attr(not(unix), expect(unused))] path: &Path,
    #[cfg_attr(not(unix), expect(unused))] bytes: &[u8],
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("Failed to create '{}'", path.display()))?;
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        if let Err(err) = written {
            let _ = fs::remove_file(path);
            return Err(err).with_context(|| format!("Failed to write '{}'", path.display()));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        bail!(
            "Owner-only file creation is not yet implemented on Windows, so the mesh identity cannot be stored safely and mesh cannot be enabled on this platform yet. This is being tracked; no workaround is available."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // `PrivateIdentity` has no `Debug`, so `unwrap_err` cannot be used on it.
    fn load_error(path: &Path) -> String {
        match load_or_mint_identity(path) {
            Ok(_) => panic!("loading '{}' must fail", path.display()),
            Err(err) => err.to_string(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_file_creates_with_mode_0600() {
        let dir = TempDir::new("owner-only");
        let path = dir.path.join("secret");

        write_owner_only_file(&path, b"abc").unwrap();

        assert_eq!(mode_of(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_file_refuses_existing_file() {
        let dir = TempDir::new("owner-only-exists");
        let path = dir.path.join("secret");
        fs::write(&path, b"first").unwrap();

        let err = write_owner_only_file(&path, b"second").unwrap_err();

        assert!(is_already_exists(&err), "{err:#}");
        assert_eq!(fs::read(&path).unwrap(), b"first");
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_file_content_round_trips() {
        let dir = TempDir::new("owner-only-content");
        let path = dir.path.join("secret");
        let bytes: Vec<u8> = (0..=255).collect();

        write_owner_only_file(&path, &bytes).unwrap();

        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn load_or_mint_identity_mints_owner_only_key_in_fresh_dir() {
        let dir = TempDir::new("identity-mint");
        let path = dir.path.join("mesh").join("identity.key");

        let identity = load_or_mint_identity(&path).unwrap();

        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(path.parent().unwrap()), 0o700);
        assert_eq!(
            fs::read(&path).unwrap(),
            identity.to_private_key_bytes(),
            "the persisted bytes must be the post-construction key so a reload matches"
        );
        assert_eq!(fingerprint(&identity).len(), 32);
    }

    #[test]
    fn fingerprint_is_the_public_address_hash_and_reveals_no_key_material() {
        let identity = PrivateIdentity::from_private_key_bytes(&[9u8; PRIVATE_KEY_LENGTH]).unwrap();

        let fingerprint = fingerprint(&identity);

        assert_eq!(
            fingerprint,
            identity.as_identity().address_hash.to_hex_string()
        );
        assert!(
            !identity.to_hex_string().contains(&fingerprint),
            "the fingerprint must not be a substring of the private key hex"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_or_mint_identity_second_call_loads_same_identity() {
        let dir = TempDir::new("identity-reload");
        let path = dir.path.join("identity.key");

        let minted = load_or_mint_identity(&path).unwrap();
        let loaded = load_or_mint_identity(&path).unwrap();

        assert_eq!(fingerprint(&minted), fingerprint(&loaded));
        assert_eq!(minted.to_private_key_bytes(), loaded.to_private_key_bytes());
    }

    #[test]
    fn load_or_mint_identity_rejects_wrong_length_naming_path() {
        let dir = TempDir::new("identity-corrupt");
        let path = dir.path.join("identity.key");
        fs::write(&path, [7u8; 10]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = load_error(&path);

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("corrupt"), "{err}");
        assert!(err.contains("found 10"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn load_or_mint_identity_rejects_group_readable_key_naming_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("identity-perms");
        let path = dir.path.join("identity.key");
        load_or_mint_identity(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let err = load_error(&path);

        assert!(
            err.contains(&format!("chmod 600 {}", path.display())),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn identity_path_mesh_dir_does_not_exist_until_mint() {
        let guard = crate::testing::TestConfigDirGuard::new("mesh-identity-path");
        let path = identity_path();
        assert_eq!(path, guard.path.join("mesh").join("identity.key"));
        assert!(
            !guard.path.join("mesh").exists(),
            "resolving the path must not create the mesh directory"
        );

        load_or_mint_identity(&path).unwrap();

        assert!(path.exists());
    }

    #[cfg(unix)]
    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[cfg(unix)]
    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    #[cfg(unix)]
    #[test]
    fn rotate_identity_replaces_the_key_and_leaves_no_trace_of_the_old_one() {
        let dir = TempDir::new("identity-rotate");
        let path = dir.path.join("mesh").join("identity.key");
        let old = load_or_mint_identity(&path).unwrap();
        let old_bytes = old.to_private_key_bytes();

        let rotation = rotate_identity(&path, &fingerprint(&old), t(1_790_000_000)).unwrap();

        assert_eq!(rotation.old_fingerprint, fingerprint(&old));
        assert_ne!(rotation.new_fingerprint, rotation.old_fingerprint);
        assert_eq!(rotation.predecessors, 1);
        let files = files_under(&dir.path);
        assert_eq!(files.len(), 2, "{files:?}");
        for file in &files {
            assert!(
                !contains_bytes(&fs::read(file).unwrap(), &old_bytes),
                "old key material survives in {}",
                file.display()
            );
        }
        assert!(!dir.path.join("mesh").join("identity.key.new").exists());
        let reloaded = load_or_mint_identity(&path).unwrap();
        assert_eq!(fingerprint(&reloaded), rotation.new_fingerprint);
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(&predecessors_path(&path)), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn rotate_identity_appends_one_predecessor_line_per_rotation() {
        let dir = TempDir::new("identity-rotate-twice");
        let path = dir.path.join("identity.key");
        let first = fingerprint(&load_or_mint_identity(&path).unwrap());

        let one = rotate_identity(&path, &first, t(1_790_000_000)).unwrap();
        let two = rotate_identity(&path, &one.new_fingerprint, t(1_790_000_060)).unwrap();

        assert_eq!(one.predecessors, 1);
        assert_eq!(two.predecessors, 2);
        assert_eq!(two.old_fingerprint, one.new_fingerprint);
        let text = fs::read_to_string(predecessors_path(&path)).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert_eq!(
            predecessors(&path).unwrap(),
            vec![
                Predecessor {
                    identity_hash: first,
                    rotated_at: "2026-09-21T14:13:20Z".to_string(),
                    reason: "rotate".to_string(),
                },
                Predecessor {
                    identity_hash: one.new_fingerprint,
                    rotated_at: "2026-09-21T14:14:20Z".to_string(),
                    reason: "rotate".to_string(),
                },
            ]
        );
    }

    #[test]
    fn rotate_identity_refuses_when_no_identity_exists() {
        let dir = TempDir::new("identity-rotate-missing");
        let path = dir.path.join("identity.key");

        let err = match rotate_identity(&path, "", t(0)) {
            Ok(_) => panic!("rotating a missing identity must fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(".mesh on"), "{err}");
        assert!(!path.exists());
        assert!(!predecessors_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn rotate_identity_refuses_a_stale_token_without_writing() {
        let dir = TempDir::new("identity-rotate-stale-token");
        let path = dir.path.join("identity.key");
        load_or_mint_identity(&path).unwrap();
        let before = fs::read(&path).unwrap();

        let err = match rotate_identity(&path, &"00".repeat(16), t(0)) {
            Ok(_) => panic!("a token for another identity must not rotate"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("changed since the dry run"), "{err}");
        assert!(err.contains(".mesh rotate"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!predecessors_path(&path).exists());
        assert!(!dir.path.join("identity.key.new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rotate_identity_refuses_a_corrupt_predecessors_file_before_replacing_the_key() {
        let dir = TempDir::new("identity-rotate-corrupt-predecessors");
        let path = dir.path.join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&path).unwrap());
        let before = fs::read(&path).unwrap();
        fs::write(predecessors_path(&path), "not json\n").unwrap();

        let err = match rotate_identity(&path, &old, t(0)) {
            Ok(_) => panic!("an unreadable history must not rotate"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("line 1"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::read_to_string(predecessors_path(&path)).unwrap(),
            "not json\n"
        );
        assert!(!dir.path.join("identity.key.new").exists());
    }

    #[test]
    fn predecessors_refuses_a_malformed_line_naming_the_file() {
        let dir = TempDir::new("identity-predecessors-corrupt");
        let path = dir.path.join("identity.key");
        let file = predecessors_path(&path);
        fs::write(
            &file,
            "{\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\nnot json\n",
        )
        .unwrap();

        let err = predecessors(&path).unwrap_err().to_string();

        assert!(err.contains(&file.display().to_string()), "{err}");
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("Fix or remove that line"), "{err}");
        fs::write(
            &file,
            "{\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\n\nnot json\n",
        )
        .unwrap();
        let err = predecessors(&path).unwrap_err().to_string();
        assert!(
            err.contains("line 3"),
            "a skipped blank line still counts towards the line number: {err}"
        );
        assert_eq!(
            predecessors(&dir.path.join("elsewhere").join("identity.key")).unwrap(),
            vec![],
            "a missing file is an empty history, not an error"
        );
    }

    #[test]
    fn predecessors_tolerates_an_unknown_field() {
        let dir = TempDir::new("identity-predecessors-unknown-field");
        let path = dir.path.join("identity.key");
        let file = predecessors_path(&path);
        fs::write(
            &file,
            "{\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\",\"note\":\"added by a newer build\"}\n",
        )
        .unwrap();

        assert_eq!(
            predecessors(&path).unwrap(),
            vec![Predecessor {
                identity_hash: "ab".to_string(),
                rotated_at: "2026-01-01T00:00:00Z".to_string(),
                reason: "rotate".to_string(),
            }]
        );

        fs::write(
            &file,
            "{\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\n",
        )
        .unwrap();
        let err = predecessors(&path).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_post_rename_append_failure_names_both_full_fingerprints() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("identity-rotate-append-fails");
        let path = dir.path.join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&path).unwrap());
        let pred = predecessors_path(&path);
        let history = "{\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\n";
        fs::write(&pred, history).unwrap();
        fs::set_permissions(&pred, fs::Permissions::from_mode(0o444)).unwrap();
        if fs::OpenOptions::new().append(true).open(&pred).is_ok() {
            return;
        }

        let err = match rotate_identity(&path, &old, t(0)) {
            Ok(_) => panic!("an unappendable history must fail the rotation"),
            Err(err) => format!("{err:#}"),
        };

        let new = fingerprint(&load_or_mint_identity(&path).unwrap());
        assert_ne!(new, old, "the key on disk is the new one");
        assert_eq!(old.len(), 32);
        assert_eq!(new.len(), 32);
        assert!(err.contains(&old), "{err}");
        assert!(err.contains(&new), "{err}");
        assert!(!dir.path.join("identity.key.new").exists());
        assert_eq!(fs::read_to_string(&pred).unwrap(), history);
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_new_file_does_not_block_rotation() {
        let dir = TempDir::new("identity-rotate-stale");
        let path = dir.path.join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&path).unwrap());
        let stale = dir.path.join("identity.key.new");
        fs::write(&stale, b"left by an interrupted rotation").unwrap();

        let rotation = rotate_identity(&path, &old, t(0)).unwrap();

        assert!(!stale.exists());
        assert_eq!(
            fingerprint(&load_or_mint_identity(&path).unwrap()),
            rotation.new_fingerprint
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_staged_key_is_removed_when_the_identity_loads() {
        let dir = TempDir::new("identity-load-stale-staged");
        let path = dir.path.join("identity.key");
        let minted = fingerprint(&load_or_mint_identity(&path).unwrap());
        let stale = dir.path.join("identity.key.new");
        fs::write(&stale, [3u8; PRIVATE_KEY_LENGTH]).unwrap();

        let loaded = fingerprint(&load_or_mint_identity(&path).unwrap());

        assert_eq!(loaded, minted, "the staged key is discarded, not adopted");
        assert!(!stale.exists(), "the staged key must not outlive the load");
        assert_eq!(files_under(&dir.path), vec![path]);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_at_the_key_is_refused_not_minted() {
        let dir = TempDir::new("identity-current-dangling-symlink");
        let link = dir.path.join("identity.key");
        std::os::unix::fs::symlink(dir.path.join("gone").join("identity.key"), &link).unwrap();
        let stale = dir.path.join("identity.key.new");
        fs::write(&stale, [3u8; PRIVATE_KEY_LENGTH]).unwrap();

        let err = match current_identity(&link) {
            Ok(_) => panic!("a dangling symlink must not load"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("regular files"), "{err}");
        assert!(!err.contains("No mesh identity"), "{err}");
        assert!(!stale.exists(), "the dry run sweeps the staged key too");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(files_under(&dir.path), vec![link], "nothing is minted");
    }

    #[cfg(unix)]
    #[test]
    fn rotate_identity_refuses_a_symlinked_key() {
        let dir = TempDir::new("identity-rotate-symlink");
        let target = dir.path.join("elsewhere").join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&target).unwrap());
        let before = fs::read(&target).unwrap();
        let link = dir.path.join("identity.key");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = match rotate_identity(&link, &old, t(0)) {
            Ok(_) => panic!("a symlinked key must not rotate"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains(&link.display().to_string()), "{err}");
        assert!(err.contains("regular files"), "{err}");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), before);
        assert!(!predecessors_path(&link).exists());
        assert!(!dir.path.join("identity.key.new").exists());

        fs::remove_file(&link).unwrap();
        fs::copy(&target, &link).unwrap();
        let history = predecessors_path(&link);
        std::os::unix::fs::symlink(dir.path.join("elsewhere").join("history"), &history).unwrap();
        let err = match rotate_identity(&link, &old, t(0)) {
            Ok(_) => panic!("a symlinked predecessors file must not rotate"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains(&history.display().to_string()), "{err}");
        assert_eq!(fs::read(&link).unwrap(), before, "the key is untouched");
    }

    #[cfg(unix)]
    #[test]
    fn append_refuses_a_symlinked_predecessors_file_atomically() {
        let dir = TempDir::new("identity-append-symlink");
        let target = dir.path.join("elsewhere");
        fs::write(&target, b"untouched\n").unwrap();
        let link = dir.path.join(PREDECESSORS_FILE);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let predecessor = Predecessor {
            identity_hash: "ab".repeat(16),
            rotated_at: "2026-01-01T00:00:00Z".to_string(),
            reason: "rotate".to_string(),
        };

        let err = append_predecessor(&link, &predecessor)
            .unwrap_err()
            .to_string();

        assert!(err.contains(&link.display().to_string()), "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"untouched\n");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
