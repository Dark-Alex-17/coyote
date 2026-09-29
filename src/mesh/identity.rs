use crate::config::paths;
use crate::mesh::lock::{read_holder_pid, write_holder_pid};
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::{mesh_config_dir, redact_hashes, rfc3339_utc, short};
#[cfg(windows)]
use crate::utils::windows_acl;

use anyhow::{Context, Result, bail};
use lxmf_core::identity::PrivateIdentity;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, TryLockError};
use std::io::{self, BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Size of the persisted key: the x25519 secret followed by the ed25519 seed, 32 bytes each.
const PRIVATE_KEY_LENGTH: usize = 64;

/// Sibling of `identity.key`: one JSON line per rotation, oldest first.
pub(crate) const PREDECESSORS_FILE: &str = "identity.predecessors.jsonl";
/// No layout of this file was ever released without a `version` field: version 1 is the
/// only one there has been, so a line without one is an unknown shape that is refused,
/// never read as version 1.
pub(crate) const PREDECESSOR_RECORD_VERSION: u64 = 1;
/// The file is the user's history, not cache, but a cheap one: a refusal says so.
const PREDECESSORS_REMEDY: Remedy =
    Remedy::UserFile("records only retired public hashes, so nothing secret is lost by editing it");

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

/// Guards `identity.key` across every Coyote process on this config dir through a kernel
/// advisory lock on the sibling `identity.key.lock`, the primitive `InstanceLock` uses. A
/// running node holds it shared for its lifetime, so sessions sharing a config dir run side
/// by side; rotation holds it exclusive, so a new key is never renamed over one a node still
/// announces under. The file is never removed, for the reason `InstanceLock` gives. Each
/// holder writes its pid into the file to word a refusal; with several shared holders the
/// pid a refusal shows is the last writer's, one of the holders.
#[derive(Debug)]
pub(crate) struct IdentityLock {
    file: File,
}

impl IdentityLock {
    /// The node's hold, refused only while a rotation holds the lock exclusively. The pid is
    /// written best effort: a Windows shared lock denies writes to every handle, the
    /// holder's included, and the pid only words a refusal.
    pub(crate) fn share(identity_path: &Path) -> Result<Self> {
        let (path, mut file) = open_lock_file(identity_path)?;
        match file.try_lock_shared() {
            Ok(()) => {
                if let Err(err) = write_holder_pid(&mut file) {
                    debug!(
                        "Mesh identity lock '{}' holds no pid: {}",
                        path.display(),
                        redact_hashes(&err.to_string())
                    );
                }
                Ok(Self { file })
            }
            Err(TryLockError::WouldBlock) => bail!(
                "The mesh identity is being rotated by another Coyote process{}. Run `.mesh on` again once it finishes.",
                holder_suffix(read_holder_pid(&mut file))
            ),
            Err(TryLockError::Error(err)) => Err(err).with_context(|| {
                format!("Failed to take the mesh identity lock '{}'", path.display())
            }),
        }
    }

    /// Rotation's hold, refused while any process holds the lock shared or exclusively.
    pub(crate) fn exclusive(identity_path: &Path) -> Result<Self> {
        let (path, mut file) = open_lock_file(identity_path)?;
        match file.try_lock() {
            Ok(()) => Self::hold(&path, file),
            Err(TryLockError::WouldBlock) => bail!(
                "A mesh node on this config dir is running in a Coyote process{}; the identity is rotated only while no session's node on this config dir is running. Run `.mesh off` in every Coyote session that shares this config dir (the holder may be another process), then rotate again.",
                holder_suffix(read_holder_pid(&mut file))
            ),
            Err(TryLockError::Error(err)) => Err(err).with_context(|| {
                format!("Failed to take the mesh identity lock '{}'", path.display())
            }),
        }
    }

    fn hold(path: &Path, mut file: File) -> Result<Self> {
        write_holder_pid(&mut file)
            .with_context(|| format!("Failed to write mesh identity lock '{}'", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for IdentityLock {
    fn drop(&mut self) {
        if let Err(err) = self.file.unlock() {
            warn!(
                "Failed to release a mesh identity lock: {}",
                redact_hashes(&err.to_string())
            );
        }
    }
}

fn holder_suffix(pid: Option<u32>) -> String {
    pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
}

/// Opens the lock file without taking the lock. The parent is created first (owner-only on
/// unix) because the lock may be taken before the first mint creates the key. The file
/// holds a pid and nothing secret, so on Windows it keeps the permissions it inherits from
/// the directory rather than the key's protected DACL.
fn open_lock_file(identity_path: &Path) -> Result<(PathBuf, File)> {
    if let Some(parent) = identity_path.parent() {
        create_private_dir(parent)?;
    }
    let path = identity_path.with_added_extension("lock");
    refuse_symlink(&path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("Failed to open mesh identity lock '{}'", path.display()))?;
    Ok((path, file))
}

/// One retired identity, as the predecessors file records it. Only the public hash is kept;
/// the private key it belonged to is gone. Unknown fields are refused, so any change to the
/// line bumps `PREDECESSOR_RECORD_VERSION` and a reader refuses the whole file on a version
/// it does not write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Predecessor {
    pub version: u64,
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
/// error, since the caller wants to name or replace the one that exists.
pub(crate) fn current_identity(path: &Path) -> Result<PrivateIdentity> {
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
/// the predecessors file. Refused while any session's node on this config dir is running,
/// which each signals by holding `IdentityLock` shared: a running node has the old key in
/// memory and its announces, links and trust checks all name it, so re-keying under it
/// would leave peers holding a destination that no longer answers. Once a node starts again
/// it announces a new destination hash under the new identity; the instance id is
/// unchanged. Peers that trusted the old identity or its destinations see a stranger and
/// must trust the new one deliberately, while this node's own trust list is unaffected.
///
/// `expected_old` is the fingerprint the caller showed the human; a key that no longer
/// matches it is left alone, since the consent was given for a different identity. The
/// predecessors file is read under the lock and before the new key is minted, so a corrupt
/// or refused history (a line of another version, or of none) fails the rotation with the
/// key still in place and the reader's own refusal and remedy as the error; a rotation never
/// appends a current line to a file the reader refuses.
///
/// The new key is written to a sibling file and renamed over `path`, so the old private key
/// is unlinked by the rename rather than overwritten or copied; the staged file is the only
/// other copy of the new key and is removed on any failure. The predecessors line is
/// appended after the rename: an interruption between the two loses the bookkeeping line,
/// never the key.
///
/// On Windows the staged key goes through the same owner-only primitive and the rename
/// keeps the moved file's own security descriptor; its DACL is protected, so the new
/// location's inheritable ACEs cannot widen it. The read-back at the end of the rotation
/// runs the same owner-only check every load does.
pub(crate) fn rotate_identity(
    path: &Path,
    expected_old: &str,
    now: SystemTime,
) -> Result<Rotation> {
    let _lock = IdentityLock::exclusive(path)?;
    remove_stale_staged_key(path)?;
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
            version: PREDECESSOR_RECORD_VERSION,
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

/// The file records only retired public hashes, nothing secret, so on Windows it keeps
/// the permissions it inherits from the directory rather than the key's protected DACL.
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
/// Each line's version is read before anything else, and a line that does not parse or is
/// not of this build's version fails the whole read, so a truncated or edited file is never
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
        let probe: VersionProbe = serde_json::from_str(&line).with_context(|| {
            unversioned_refusal(
                "identity predecessors file",
                &path,
                Some(index + 1),
                PREDECESSOR_RECORD_VERSION,
                PREDECESSORS_REMEDY,
            )
        })?;
        if probe.version != PREDECESSOR_RECORD_VERSION {
            bail!(version_refusal(
                "identity predecessors file",
                &path,
                Some(index + 1),
                probe.version,
                PREDECESSOR_RECORD_VERSION,
                PREDECESSORS_REMEDY
            ));
        }
        let predecessor: Predecessor = serde_json::from_str(&line).with_context(|| {
            format!(
                "Mesh identity predecessors file '{}' line {} is not a version-{PREDECESSOR_RECORD_VERSION} record. {}",
                path.display(),
                index + 1,
                PREDECESSORS_REMEDY.sentence()
            )
        })?;
        out.push(predecessor);
    }
    Ok(out)
}

/// Loads the identity at `path`, minting and persisting a fresh one if none exists yet.
/// A key that is the wrong length, readable by other users, or on Windows not owner-only
/// is refused rather than used silently.
pub(crate) fn load_or_mint_identity(path: &Path) -> Result<PrivateIdentity> {
    remove_stale_staged_key(path)?;
    if path.exists() {
        return load_identity(path);
    }

    let bytes = rand::random::<[u8; PRIVATE_KEY_LENGTH]>();
    let identity = PrivateIdentity::from_private_key_bytes(&bytes)
        .expect("64 random bytes are a valid identity");
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
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

/// On Windows the directory inherits its parent's DACL and the key inside protects itself
/// with a protected DACL of its own, so the unix 0700 has no counterpart there.
fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("Failed to create directory '{}'", dir.display()))
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

    #[cfg(not(windows))]
    let bytes = fs::read(path)
        .with_context(|| format!("Failed to read mesh identity file '{}'", path.display()))?;
    #[cfg(windows)]
    let bytes = read_owner_only_key(path)?;
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

/// Windows counterpart of the unix mode check in `load_identity`, reading the key through
/// the handle the volume was queried on. A volume without persistent ACLs (FAT32, exFAT)
/// reports every file as open to Everyone, so refusing there would contradict the
/// warn-and-proceed the mint made on the same volume; the warning is repeated instead and
/// the DACL check skipped. A key the current user cannot read at all (an empty protected
/// DACL, or one that names other users only) is inspected through a `READ_CONTROL`-only
/// handle, so its refusal names the DACL and the remedy rather than a bare access error.
#[cfg(windows)]
fn read_owner_only_key(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;

    let read_context = || format!("Failed to read mesh identity file '{}'", path.display());
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            let handle = windows_acl::open_for_security_read(path).with_context(|| {
                format!(
                    "Failed to read the permissions of '{}' after it refused to open for reading. Delete the file to mint a new identity; any trust other peers hold for the old identity is lost.",
                    path.display()
                )
            })?;
            if let Err(problem) = owner_only_verdict(path, &handle)? {
                bail!(owner_only_refusal(path, problem)?);
            }
            return Err(err).with_context(read_context);
        }
        Err(err) => return Err(err).with_context(read_context),
    };
    let dacl_is_meaningful = match windows_acl::volume_acls(&file) {
        Ok(volume) => {
            match acl_less_volume_warning(path, &volume.fs_name, volume.persistent_acls) {
                Some(warning) => {
                    warn!("{}", redact_hashes(&warning));
                    false
                }
                None => true,
            }
        }
        Err(err) => {
            debug!(
                "Could not query the volume holding '{}': {}",
                path.display(),
                redact_hashes(&err.to_string())
            );
            true
        }
    };
    if dacl_is_meaningful && let Err(problem) = owner_only_verdict(path, &file)? {
        bail!(owner_only_refusal(path, problem)?);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).with_context(read_context)?;
    Ok(bytes)
}

#[cfg(windows)]
fn owner_only_verdict(
    path: &Path,
    file: &File,
) -> Result<Result<(), windows_acl::OwnerOnlyProblem>> {
    windows_acl::check_owner_only(file)
        .with_context(|| format!("Failed to read the permissions of '{}'", path.display()))
}

/// The one refusal both open paths of `read_owner_only_key` print, so the message a user
/// sees cannot depend on whether the key was readable.
#[cfg(windows)]
fn owner_only_refusal(path: &Path, problem: windows_acl::OwnerOnlyProblem) -> Result<String> {
    let sid = windows_acl::current_user_sid_string()
        .context("Failed to look up the current user's SID")?;
    let owner_is_wrong = problem == windows_acl::OwnerOnlyProblem::OwnerIsNotCurrentUser;
    let remedy = render_remedy(&owner_only_remedy(path, &sid, owner_is_wrong));
    let elevation = if owner_is_wrong {
        " The commands must be run from an elevated (Administrator) prompt: taking ownership needs SeTakeOwnershipPrivilege."
    } else {
        ""
    };
    Ok(format!(
        "Mesh identity file '{}' {problem}. A private key other users can read must not be used. Run {remedy} and try again, or delete the file to mint a new identity; any trust other peers hold for the old identity is lost.{elevation}",
        path.display()
    ))
}

/// The `icacls` invocations, as argument lists after the program name, that make `path`
/// owner-only again whatever was found wrong with it. `/reset` drops every explicit ACE,
/// which `/inheritance:r` (inherited ACEs only) and `/grant:r` (the named SID's ACEs only)
/// would each leave in place for another principal; the last command then cuts
/// inheritance and adds the single ACE. Ownership comes first, since an owner always holds
/// the `WRITE_DAC` the other two need, and it alone needs elevation: `/setowner` on a file
/// another user owns takes `SeTakeOwnershipPrivilege`, which only an elevated prompt holds.
#[cfg(any(windows, test))]
fn owner_only_remedy(path: &Path, sid: &str, owner_is_wrong: bool) -> Vec<Vec<String>> {
    let path = path.display().to_string();
    let mut commands = Vec::with_capacity(3);
    if owner_is_wrong {
        commands.push(vec![path.clone(), "/setowner".into(), format!("*{sid}")]);
    }
    commands.push(vec![path.clone(), "/reset".into()]);
    commands.push(vec![
        path,
        "/inheritance:r".into(),
        "/grant:r".into(),
        format!("*{sid}:F"),
    ]);
    commands
}

/// Renders `owner_only_remedy` as separate commands, since PowerShell 5.1 has no `&&`.
/// The path is the first argument of each and the only one that can hold a space.
#[cfg(any(windows, test))]
fn render_remedy(commands: &[Vec<String>]) -> String {
    commands
        .iter()
        .map(|argv| format!("`icacls \"{}\" {}`", argv[0], argv[1..].join(" ")))
        .collect::<Vec<_>>()
        .join(", then ")
}

/// Worded once for mint and load: a volume that keeps no ACLs cannot hold the DACL that
/// makes the key owner-only, so the key is exactly as private as the volume is.
#[cfg(any(windows, test))]
fn acl_less_volume_warning(path: &Path, fs_name: &str, persistent_acls: bool) -> Option<String> {
    if persistent_acls {
        return None;
    }
    Some(format!(
        "Mesh identity file '{}' is on a {fs_name} volume, which stores no file permissions: every user of this machine can read the key. Keep the config dir on an NTFS or ReFS volume to keep the key private.",
        path.display()
    ))
}

fn is_already_exists(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|io| io.kind() == ErrorKind::AlreadyExists)
}

/// Creates `path` with `bytes` so that only the owner can read it at any point in its life.
/// The file must not already exist; a concurrent creator surfaces as `ErrorKind::AlreadyExists`.
///
/// This is a second primitive beside `write_file_atomic` rather than a mode on it: a temp
/// file renamed into place carries the temp file's permissions, on Windows its security
/// descriptor, so the key has to be created with its final permissions in the one call
/// that creates it. On unix that is `create_new` with mode 0600; on Windows it is
/// `CreateFileW` with a protected DACL holding a single allow ACE for the current user.
/// The two are parity, not absolute protection: a local Administrator can take ownership
/// of the file, the same limit 0600 has against root.
pub(crate) fn write_owner_only_file(
    #[cfg_attr(not(any(unix, windows)), expect(unused))] path: &Path,
    #[cfg_attr(not(any(unix, windows)), expect(unused))] bytes: &[u8],
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
    #[cfg(windows)]
    {
        let mut file = windows_acl::create_owner_only(path)
            .with_context(|| format!("Failed to create '{}'", path.display()))?;
        match windows_acl::volume_acls(&file) {
            Ok(volume) => {
                if let Some(warning) =
                    acl_less_volume_warning(path, &volume.fs_name, volume.persistent_acls)
                {
                    warn!("{}", redact_hashes(&warning));
                }
            }
            Err(err) => debug!(
                "Could not query the volume holding '{}': {}",
                path.display(),
                redact_hashes(&err.to_string())
            ),
        }
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        if let Err(err) = written {
            // The handle was opened with no sharing, so the file cannot be removed while
            // it is open.
            drop(file);
            let _ = fs::remove_file(path);
            return Err(err).with_context(|| format!("Failed to write '{}'", path.display()));
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!(
            "Owner-only file creation is not yet implemented on this platform, so the mesh identity cannot be stored safely and mesh cannot be enabled here yet. This is being tracked; no workaround is available."
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
    fn acl_less_volume_warning_names_the_path_and_filesystem_only_without_persistent_acls() {
        let path = Path::new("C:\\Users\\me\\.coyote\\mesh\\identity.key");

        let fat = acl_less_volume_warning(path, "FAT32", false).unwrap();
        assert!(fat.contains("identity.key"), "{fat}");
        assert!(fat.contains("FAT32"), "{fat}");
        assert!(fat.is_ascii(), "{fat}");
        let exfat = acl_less_volume_warning(path, "exFAT", false).unwrap();
        assert!(exfat.contains("exFAT"), "{exfat}");

        assert!(acl_less_volume_warning(path, "NTFS", true).is_none());
        assert!(acl_less_volume_warning(path, "ReFS", true).is_none());
    }

    #[test]
    fn owner_only_remedy_resets_explicit_aces_before_cutting_inheritance() {
        let path = Path::new("C:\\Users\\me\\My Files\\.coyote\\mesh\\identity.key");
        let sid = "S-1-5-21-1-2-3-1001";
        let path_arg = path.display().to_string();

        let commands = owner_only_remedy(path, sid, false);
        assert_eq!(
            commands,
            vec![
                vec![path_arg.clone(), "/reset".to_string()],
                vec![
                    path_arg.clone(),
                    "/inheritance:r".to_string(),
                    "/grant:r".to_string(),
                    format!("*{sid}:F"),
                ],
            ]
        );

        let with_owner = owner_only_remedy(path, sid, true);
        assert_eq!(with_owner.len(), 3);
        assert_eq!(
            with_owner[0],
            vec![path_arg.clone(), "/setowner".to_string(), format!("*{sid}")]
        );
        assert_eq!(with_owner[1..], commands[..]);

        let rendered = render_remedy(&with_owner);
        assert_eq!(
            rendered,
            format!(
                "`icacls \"{path_arg}\" /setowner *{sid}`, then `icacls \"{path_arg}\" /reset`, then `icacls \"{path_arg}\" /inheritance:r /grant:r *{sid}:F`"
            )
        );
        assert!(!rendered.contains("&&"), "{rendered}");
    }

    #[cfg(windows)]
    fn assert_owner_only(path: &Path) {
        let file = File::open(path).unwrap();
        let summary = windows_acl::inspect(&file).unwrap();
        assert!(summary.owner_is_current_user, "{summary:?}");
        assert!(summary.dacl_present, "{summary:?}");
        assert!(
            summary.protected,
            "SE_DACL_PROTECTED must be set: {summary:?}"
        );
        assert_eq!(summary.aces.len(), 1, "{summary:?}");
        assert!(summary.aces[0].allows, "{summary:?}");
        assert!(summary.aces[0].current_user, "{summary:?}");
        assert_eq!(windows_acl::check_owner_only(&file).unwrap(), Ok(()));
    }

    #[cfg(windows)]
    #[test]
    fn write_owner_only_file_creates_a_protected_single_ace_dacl_for_the_current_user() {
        let dir = TempDir::new("owner-only-dacl");
        let path = dir.path.join("secret");
        let bytes: Vec<u8> = (0..=255).collect();

        write_owner_only_file(&path, &bytes).unwrap();

        assert_owner_only(&path);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[cfg(windows)]
    #[test]
    fn write_owner_only_file_refuses_an_existing_file_on_windows() {
        let dir = TempDir::new("owner-only-exists-dacl");
        let path = dir.path.join("secret");
        fs::write(&path, b"first").unwrap();

        let err = write_owner_only_file(&path, b"second").unwrap_err();

        assert!(is_already_exists(&err), "{err:#}");
        assert_eq!(fs::read(&path).unwrap(), b"first");
    }

    #[cfg(windows)]
    #[test]
    fn load_or_mint_identity_mints_an_owner_only_key_and_reloads_it_on_windows() {
        let dir = TempDir::new("identity-mint-dacl");
        let path = dir.path.join("mesh").join("identity.key");

        let minted = load_or_mint_identity(&path).unwrap();

        assert_owner_only(&path);
        assert_eq!(fs::read(&path).unwrap(), minted.to_private_key_bytes());
        let loaded = load_or_mint_identity(&path).unwrap();
        assert_eq!(fingerprint(&minted), fingerprint(&loaded));
        assert_eq!(minted.to_private_key_bytes(), loaded.to_private_key_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn load_or_mint_identity_refuses_an_unprotected_everyone_dacl_naming_icacls() {
        let dir = TempDir::new("identity-dacl-everyone");
        let path = dir.path.join("identity.key");
        load_or_mint_identity(&path).unwrap();
        windows_acl::widen_with_sddl(&path, "D:(A;;GA;;;WD)").unwrap();
        assert_eq!(
            windows_acl::check_owner_only(&File::open(&path).unwrap()).unwrap(),
            Err(windows_acl::OwnerOnlyProblem::DaclInherits)
        );

        let err = load_error(&path);

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("icacls"), "{err}");
        assert!(err.contains("/reset"), "{err}");
        assert!(err.contains("/inheritance:r"), "{err}");
        assert!(err.contains("*S-1-"), "{err}");
    }

    // `OwnerIsNotCurrentUser` is absent: setting it up takes a second account, so the
    // `/setowner` step is covered by the argv unit test alone.
    #[cfg(windows)]
    #[test]
    fn applying_the_printed_remedy_cures_every_reachable_dacl_problem() {
        use windows_acl::OwnerOnlyProblem;

        let sid = windows_acl::current_user_sid_string().unwrap();
        let cases = [
            (
                format!("D:P(A;;GA;;;WD)(A;;GA;;;{sid})"),
                OwnerOnlyProblem::ForeignAce,
            ),
            ("D:(A;;GA;;;WD)".to_string(), OwnerOnlyProblem::DaclInherits),
            (
                "D:NO_ACCESS_CONTROL".to_string(),
                OwnerOnlyProblem::DaclMissing,
            ),
            ("D:P".to_string(), OwnerOnlyProblem::NoAceForCurrentUser),
        ];

        for (sddl, expected) in cases {
            let dir = TempDir::new("identity-dacl-remedy");
            let path = dir.path.join("identity.key");
            let minted = load_or_mint_identity(&path).unwrap();
            windows_acl::widen_with_sddl(&path, &sddl).unwrap();
            let handle = windows_acl::open_for_security_read(&path).unwrap();
            assert_eq!(
                windows_acl::check_owner_only(&handle).unwrap(),
                Err(expected),
                "{sddl}"
            );

            let err = load_error(&path);
            assert!(err.contains(&path.display().to_string()), "{sddl}: {err}");
            assert!(err.contains("icacls"), "{sddl}: {err}");
            assert!(err.contains("/reset"), "{sddl}: {err}");
            assert!(err.contains("/inheritance:r"), "{sddl}: {err}");

            for argv in owner_only_remedy(&path, &sid, false) {
                let output = std::process::Command::new("icacls")
                    .args(&argv)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{sddl}: icacls {argv:?} failed: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            assert_owner_only(&path);
            let loaded = load_or_mint_identity(&path).unwrap();
            assert_eq!(fingerprint(&minted), fingerprint(&loaded), "{sddl}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn write_owner_only_file_refuses_a_symlink_instead_of_creating_its_target() {
        let dir = TempDir::new("owner-only-symlink");
        let target = dir.path.join("target");
        let path = dir.path.join("secret");
        if let Err(err) = std::os::windows::fs::symlink_file(&target, &path) {
            if err.raw_os_error() == Some(1314) {
                eprintln!(
                    "skipping: this account cannot create symlinks (ERROR_PRIVILEGE_NOT_HELD)"
                );
                return;
            }
            panic!("symlink_file failed: {err}");
        }

        let err = write_owner_only_file(&path, b"x").unwrap_err();

        assert!(is_already_exists(&err), "{err:#}");
        assert!(!target.exists(), "the symlink target must not be created");
    }

    #[cfg(windows)]
    #[test]
    fn rotate_identity_leaves_the_renamed_key_owner_only_on_windows() {
        let dir = TempDir::new("identity-rotate-dacl");
        let path = dir.path.join("mesh").join("identity.key");
        let old = load_or_mint_identity(&path).unwrap();

        let rotation = rotate_identity(&path, &fingerprint(&old), t(1_790_000_000)).unwrap();

        assert_owner_only(&path);
        assert!(!dir.path.join("mesh").join("identity.key.new").exists());
        let reloaded = load_or_mint_identity(&path).unwrap();
        assert_eq!(fingerprint(&reloaded), rotation.new_fingerprint);
        assert_ne!(rotation.new_fingerprint, fingerprint(&old));
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
        write_owner_only_file(&path, &[7u8; 10]).unwrap();

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
        assert_eq!(files.len(), 3, "key, predecessors and lock: {files:?}");
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
        assert_eq!(mode_of(&path.with_added_extension("lock")), 0o600);
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
                    version: PREDECESSOR_RECORD_VERSION,
                    identity_hash: first,
                    rotated_at: "2026-09-21T14:13:20Z".to_string(),
                    reason: "rotate".to_string(),
                },
                Predecessor {
                    version: PREDECESSOR_RECORD_VERSION,
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

    #[cfg(unix)]
    #[test]
    fn rotate_identity_refuses_an_unversioned_predecessors_line_and_appends_nothing() {
        let dir = TempDir::new("identity-rotate-unversioned-predecessors");
        let path = dir.path.join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&path).unwrap());
        let before = fs::read(&path).unwrap();
        let unversioned = "{\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\n";
        fs::write(predecessors_path(&path), unversioned).unwrap();

        let err = match rotate_identity(&path, &old, t(0)) {
            Ok(_) => panic!("a refused history must not rotate"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::read_to_string(predecessors_path(&path)).unwrap(),
            unversioned,
            "no version-1 line is appended to a file the reader refuses"
        );
        assert!(!dir.path.join("identity.key.new").exists());
    }

    /// One current-version history line, with `extra` spliced in as further JSON members.
    fn history_line(extra: &str) -> String {
        format!(
            "{{\"version\":{PREDECESSOR_RECORD_VERSION},\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"{extra}}}\n"
        )
    }

    #[test]
    fn predecessors_refuses_a_malformed_line_naming_the_file() {
        let dir = TempDir::new("identity-predecessors-corrupt");
        let path = dir.path.join("identity.key");
        let file = predecessors_path(&path);
        fs::write(&file, format!("{}not json\n", history_line(""))).unwrap();

        let err = format!("{:#}", predecessors(&path).unwrap_err());

        assert!(err.contains(&file.display().to_string()), "{err}");
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(err.contains("retired public hashes"), "{err}");
        fs::write(&file, format!("{}\nnot json\n", history_line(""))).unwrap();
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
    fn predecessors_refuses_an_unknown_field() {
        let dir = TempDir::new("identity-predecessors-unknown-field");
        let path = dir.path.join("identity.key");
        let file = predecessors_path(&path);
        fs::write(&file, history_line("")).unwrap();
        assert_eq!(
            predecessors(&path).unwrap(),
            vec![Predecessor {
                version: PREDECESSOR_RECORD_VERSION,
                identity_hash: "ab".to_string(),
                rotated_at: "2026-01-01T00:00:00Z".to_string(),
                reason: "rotate".to_string(),
            }]
        );

        fs::write(&file, history_line(",\"note\":\"added by hand\"")).unwrap();
        let err = format!("{:#}", predecessors(&path).unwrap_err());
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("is not a version-1 record"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(err.contains("retired public hashes"), "{err}");
        assert!(err.contains("note"), "{err}");

        fs::write(&file, format!("{{\"version\":{PREDECESSOR_RECORD_VERSION},\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}}\n")).unwrap();
        let err = format!("{:#}", predecessors(&path).unwrap_err());
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("identity_hash"), "{err}");
    }

    #[test]
    fn predecessors_refuses_a_newer_line_and_shows_none_of_the_history() {
        let dir = TempDir::new("identity-predecessors-newer");
        let path = dir.path.join("identity.key");
        let file = predecessors_path(&path);
        let newer = format!(
            "{{\"version\":{},\"identity_hash\":\"cd\",\"rotated_at\":\"2026-01-02T00:00:00Z\",\"reason\":\"rotate\",\"later\":1}}\n",
            PREDECESSOR_RECORD_VERSION + 1
        );
        fs::write(&file, format!("{}{newer}", history_line(""))).unwrap();

        let err = predecessors(&path).unwrap_err().to_string();

        assert!(err.contains(&file.display().to_string()), "{err}");
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("version 2"), "{err}");
        assert!(err.contains("version 1"), "{err}");
        assert!(err.contains("upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(err.contains("retired public hashes"), "{err}");
        assert!(
            !err.contains("\"ab\""),
            "the good line is neither shown nor quoted: {err}"
        );
    }

    #[test]
    fn predecessors_refuses_a_pre_baseline_line_as_having_no_migration() {
        let dir = TempDir::new("identity-predecessors-older");
        let path = dir.path.join("identity.key");
        let file = predecessors_path(&path);
        fs::write(
            &file,
            "{\"version\":0,\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\n",
        )
        .unwrap();

        let err = predecessors(&path).unwrap_err().to_string();

        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("version 0"), "{err}");
        assert!(err.contains("no migration"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(!err.contains("upgrade Coyote"), "{err}");
    }

    #[test]
    fn predecessors_refuses_a_line_without_a_version() {
        let dir = TempDir::new("identity-predecessors-unversioned");
        let path = dir.path.join("identity.key");
        let file = predecessors_path(&path);
        fs::write(
            &file,
            "{\"identity_hash\":\"ab\",\"rotated_at\":\"2026-01-01T00:00:00Z\",\"reason\":\"rotate\"}\n",
        )
        .unwrap();

        let err = format!("{:#}", predecessors(&path).unwrap_err());

        assert!(err.contains(&file.display().to_string()), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_post_rename_append_failure_names_both_full_fingerprints() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("identity-rotate-append-fails");
        let path = dir.path.join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&path).unwrap());
        let pred = predecessors_path(&path);
        let history = history_line("");
        fs::write(&pred, &history).unwrap();
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
        assert!(
            stale.exists(),
            "naming the identity must not sweep the staged key"
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let mut files = files_under(&dir.path);
        files.sort();
        assert_eq!(files, vec![link, stale], "nothing is minted");
    }

    #[cfg(unix)]
    #[test]
    fn a_dry_run_leaves_a_stale_staged_key_alone_and_rotation_sweeps_it() {
        let dir = TempDir::new("identity-dry-run-stale-staged");
        let path = dir.path.join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&path).unwrap());
        let stale = dir.path.join("identity.key.new");
        fs::write(&stale, [3u8; PRIVATE_KEY_LENGTH]).unwrap();

        let named = fingerprint(&current_identity(&path).unwrap());

        assert_eq!(named, old);
        assert!(stale.exists(), "a dry run changes nothing");

        let rotation = rotate_identity(&path, &old, t(0)).unwrap();

        assert!(!stale.exists(), "rotation sweeps the staged key first");
        assert_eq!(
            fingerprint(&load_or_mint_identity(&path).unwrap()),
            rotation.new_fingerprint
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotate_identity_is_refused_while_a_node_holds_the_identity_lock() {
        let dir = TempDir::new("identity-rotate-locked");
        let path = dir.path.join("identity.key");
        let old = fingerprint(&load_or_mint_identity(&path).unwrap());
        let before = fs::read(&path).unwrap();
        let _node = IdentityLock::share(&path).unwrap();

        let err = match rotate_identity(&path, &old, t(0)) {
            Ok(_) => panic!("rotating under a running node must fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains(".mesh off"), "{err}");
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!dir.path.join("identity.key.new").exists());
        assert!(!predecessors_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn identity_lock_is_shared_between_nodes_and_exclusive_for_rotation() {
        let dir = TempDir::new("identity-lock-modes");
        let path = dir.path.join("mesh").join("identity.key");
        let lock_path = dir.path.join("mesh").join("identity.key.lock");

        let first = IdentityLock::share(&path).unwrap();
        let second = IdentityLock::share(&path).unwrap();

        assert_eq!(mode_of(path.parent().unwrap()), 0o700);
        assert_eq!(mode_of(&lock_path), 0o600);
        assert!(IdentityLock::exclusive(&path).is_err());
        drop(first);
        assert!(
            IdentityLock::exclusive(&path).is_err(),
            "one remaining node still refuses rotation"
        );
        drop(second);

        let rotation = IdentityLock::exclusive(&path).unwrap();

        let err = IdentityLock::share(&path).unwrap_err().to_string();
        assert!(err.contains("rotated"), "{err}");
        assert!(err.contains(".mesh on"), "{err}");
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "{err}"
        );
        drop(rotation);
        assert!(
            lock_path.exists(),
            "releasing must not unlink the lock file"
        );
        let _again = IdentityLock::share(&path).unwrap();
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
            version: PREDECESSOR_RECORD_VERSION,
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
