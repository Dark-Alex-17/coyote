use crate::config::paths;

use anyhow::{Context, Result, bail};
use lxmf_core::identity::PrivateIdentity;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// Size of the persisted key: the x25519 secret followed by the ed25519 seed, 32 bytes each.
const PRIVATE_KEY_LENGTH: usize = 64;

/// Where this config dir keeps its mesh identity.
pub(crate) fn identity_path() -> PathBuf {
    paths::config_dir().join("mesh").join("identity.key")
}

/// The identity's address hash as hex. This is what peers see, so it is safe to log.
pub(crate) fn fingerprint(identity: &PrivateIdentity) -> String {
    identity.address_hash().to_hex_string()
}

/// Loads the identity at `path`, minting and persisting a fresh one if none exists yet.
/// A key that is the wrong length or, on unix, readable by other users is refused rather
/// than used silently.
pub(crate) fn load_or_mint_identity(path: &Path) -> Result<PrivateIdentity> {
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
            debug!("Minted mesh identity {}", fingerprint(&identity));
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
        use std::io::Write;
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
}
