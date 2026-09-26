//! The keystore: exactly one Host identity.
//!
//! The identity is one 32-byte [`SecretKey`] in `keys/identity.seed`, mode
//! `0600`. Seeds never leave the daemon: callers get only public material
//! ([`IdInfo`]) or, internally, a built [`NodeKeys`] for signing. A new identity
//! means a new Host.
//!
//! Authoritative files are never truncated in place. A new seed is published
//! with a non-overwriting link from a synced private temporary file, so an
//! existing seed is never replaced. An interrupted publication is rejected on
//! the next open instead of being guessed at.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, anyhow};
use arena0_api::IdInfo;
use arena0_crypto::{NodeKeys, SecretKey};
use arena0_protocol::{PeerId, PeerIdSource};
use rand::RngCore as _;
use rand::rngs::OsRng;
use zeroize::Zeroizing;

const SEED_FILE: &str = "identity.seed";
const PRIVATE_FILE_MODE: u32 = 0o600;
const PRIVATE_DIR_MODE: u32 = 0o700;

// Temporary names are deliberately recognizable so open() can reject an
// interrupted publication rather than silently ignoring a possibly newer seed.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The Host's identity custody.
///
/// The seed is read exactly once by [`Keystore::open`] or minted exactly once
/// by [`Keystore::create`]. Every identity view derives from that loaded
/// snapshot, so a later on-disk seed change cannot split one Host across two
/// identities.
pub struct Keystore {
    keys: Arc<NodeKeys>,
}

impl std::fmt::Debug for Keystore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Keystore")
            .field("peer_id", &self.peer_id())
            .field("transport_key", &self.keys.ed25519_public_key())
            .finish_non_exhaustive()
    }
}

impl Keystore {
    /// Open the Host identity; `Ok(None)` if `keys/identity.seed` does not exist.
    pub fn open(keys_dir: PathBuf) -> anyhow::Result<Option<Self>> {
        crate::paths::validate_path(&keys_dir).context("validate keystore path")?;
        validate_keys_dir(&keys_dir)?;
        scan_keys_dir(&keys_dir)?;
        match load_crypto(&keys_dir)? {
            Some(crypto) => Ok(Some(Self {
                keys: Arc::new(crypto),
            })),
            None => Ok(None),
        }
    }

    /// Mint the Host identity; fails if one already exists.
    pub fn create(keys_dir: PathBuf) -> anyhow::Result<Self> {
        crate::paths::validate_path(&keys_dir).context("validate keystore path")?;
        validate_keys_dir(&keys_dir)?;
        scan_keys_dir(&keys_dir)?;

        let seed = generate_seed()?;
        write_seed_bytes(&keys_dir.join(SEED_FILE), &seed)?;
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes(*seed));
        Ok(Self {
            keys: Arc::new(crypto),
        })
    }

    /// The Host's durable `PeerId`.
    #[must_use]
    pub fn peer_id(&self) -> PeerId {
        self.keys.peer_id()
    }

    /// Custody-side signing keys for the loaded Host identity snapshot.
    #[must_use]
    pub fn node_keys(&self) -> Arc<NodeKeys> {
        Arc::clone(&self.keys)
    }

    /// Public material for the Host identity.
    #[must_use]
    pub fn info(&self) -> IdInfo {
        IdInfo {
            peer_id: self.peer_id(),
            transport_key: self.keys.ed25519_public_key(),
        }
    }
}

fn generate_seed() -> anyhow::Result<Zeroizing<[u8; 32]>> {
    let mut seed = Zeroizing::new([0u8; 32]);
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut *seed)
        .context("generate identity seed from the operating-system CSPRNG")?;
    anyhow::ensure!(
        seed.iter().any(|byte| *byte != 0),
        "operating-system CSPRNG returned an all-zero identity seed"
    );
    Ok(seed)
}

fn validate_keys_dir(keys_dir: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(keys_dir)
        .with_context(|| format!("inspect keystore directory {}", keys_dir.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "keystore path must be a directory and must not be a symlink: {}",
        keys_dir.display()
    );

    // Tighten an existing directory before any key material is read. Failure
    // to tighten is a fail-closed open error.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o7777 != PRIVATE_DIR_MODE {
            fs::set_permissions(keys_dir, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
                .with_context(|| {
                    format!("set keystore directory mode for {}", keys_dir.display())
                })?;
        }
        let checked = fs::symlink_metadata(keys_dir)?;
        anyhow::ensure!(
            !checked.file_type().is_symlink()
                && checked.is_dir()
                && checked.permissions().mode() & 0o7777 == PRIVATE_DIR_MODE,
            "keystore directory is not private: {}",
            keys_dir.display()
        );
    }
    Ok(())
}

/// Reject every entry that is not the Host identity seed. A recognizable
/// temporary means an interrupted publication; anything else is unsupported.
fn scan_keys_dir(keys_dir: &Path) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(keys_dir)
        .context("read keys directory")?
        .collect::<Result<Vec<_>, _>>()
        .context("read keys directory entry")?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(SEED_FILE) {
            continue;
        }
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("keystore entry name is not valid UTF-8"))?;
        anyhow::bail!("unexpected keystore entry {name:?}; interrupted or unsupported state");
    }
    Ok(())
}

fn load_crypto(keys_dir: &Path) -> anyhow::Result<Option<NodeKeys>> {
    let path = keys_dir.join(SEED_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect seed file {}", path.display()));
        }
    };
    ensure_private_regular(&path, &metadata, "seed file")?;
    let bytes = Zeroizing::new(
        fs::read(&path).with_context(|| format!("read seed file {}", path.display()))?,
    );
    let seed: Zeroizing<[u8; 32]> = Zeroizing::new(
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("seed file is not exactly 32 bytes"))?,
    );
    anyhow::ensure!(
        seed.iter().any(|byte| *byte != 0),
        "seed file contains an all-zero identity secret"
    );
    Ok(Some(NodeKeys::from_secret(SecretKey::from_bytes(*seed))))
}

pub(crate) fn ensure_private_regular(
    path: &Path,
    metadata: &Metadata,
    kind: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "{kind} must be a regular non-symlink file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o7777 == PRIVATE_FILE_MODE,
            "{kind} must have mode 0600: {}",
            path.display()
        );
    }
    Ok(())
}

/// Write a seed file with owner-only permissions (`0600`) without ever
/// replacing an existing path.
fn write_seed_bytes(path: &Path, seed: &[u8; 32]) -> anyhow::Result<()> {
    anyhow::ensure!(
        seed.iter().any(|byte| *byte != 0),
        "refusing to persist an all-zero identity seed"
    );
    publish_new_private(path, seed, "seed file")
}

/// Publish a brand-new private file. A hard link is the non-overwriting atomic
/// publication primitive available through the standard Unix filesystem API:
/// an existing regular file, directory, or symlink makes the link fail with
/// `AlreadyExists` rather than being replaced.
pub(crate) fn publish_new_private(path: &Path, bytes: &[u8], kind: &str) -> anyhow::Result<()> {
    crate::paths::validate_path(path).context("validate private file path")?;
    ensure_absent(path, kind)?;

    let (temp_path, mut temp_file) = create_private_temp(path)?;
    let result = (|| {
        temp_file
            .write_all(bytes)
            .context("write private temporary file")?;
        temp_file
            .sync_all()
            .context("sync private temporary file")?;
        drop(temp_file);

        match fs::hard_link(&temp_path, path) {
            Ok(()) => {}
            Err(error) => {
                return Err(anyhow::Error::new(error).context("publish private file"));
            }
        }
        sync_parent_dir(path)?;
        fs::remove_file(&temp_path).context("remove private temporary file")?;
        sync_parent_dir(path)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = cleanup_temp(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn create_private_temp(path: &Path) -> anyhow::Result<(PathBuf, File)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    crate::paths::validate_path(parent).context("validate temporary file directory")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("private file name is not valid UTF-8"))?;

    for _ in 0..32 {
        let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".{name}.tmp.{}.{}", std::process::id(), serial);
        let temp_path = parent.join(temp_name);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(PRIVATE_FILE_MODE);
        }
        match options.open(&temp_path) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(error) =
                        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
                    {
                        drop(file);
                        let _ = fs::remove_file(&temp_path);
                        return Err(error).with_context(|| {
                            format!("set private temporary file mode {}", temp_path.display())
                        });
                    }
                }
                return Ok((temp_path, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create private temporary file {}", temp_path.display())
                });
            }
        }
    }
    anyhow::bail!("could not allocate a unique private temporary file")
}

fn ensure_absent(path: &Path, kind: &str) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::bail!(
            "refusing to overwrite existing {kind} path ({}: {:?})",
            path.display(),
            metadata.file_type()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {kind} path {}", path.display())),
    }
}

fn cleanup_temp(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent_dir(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove temporary file {}", path.display()))
        }
    }
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    crate::paths::validate_path(parent).context("validate parent directory")?;
    let metadata = fs::symlink_metadata(parent).context("inspect parent directory")?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "parent path is not a regular directory: {}",
        parent.display()
    );
    let directory = OpenOptions::new()
        .read(true)
        .open(parent)
        .with_context(|| format!("open parent directory {} for syncing", parent.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("sync parent directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("keys")).unwrap();
        dir
    }

    fn write_private(path: &Path, bytes: &[u8]) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(PRIVATE_FILE_MODE);
        }
        let mut file = options.open(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn create_open_round_trip() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");

        let created = Keystore::create(keys.clone()).unwrap();
        let peer_id = created.peer_id();
        assert_eq!(created.info().peer_id, peer_id);
        assert_eq!(
            created.info().transport_key,
            created.node_keys().ed25519_public_key()
        );
        assert_eq!(PeerId::from_ed25519(&created.info().transport_key), peer_id);

        let opened = Keystore::open(keys).unwrap().expect("identity should open");
        assert_eq!(opened.peer_id(), peer_id);
        assert_eq!(opened.info(), created.info());
        assert_eq!(opened.node_keys().peer_id(), peer_id);
    }

    #[test]
    fn open_returns_none_without_identity() {
        let dir = keys_dir();
        assert!(Keystore::open(dir.path().join("keys")).unwrap().is_none());
    }

    #[test]
    fn create_refuses_a_second_identity() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");

        let first = Keystore::create(keys.clone()).unwrap();
        let error = Keystore::create(keys.clone()).unwrap_err();
        assert!(error.to_string().contains("overwrite"), "{error}");
        assert_eq!(
            Keystore::open(keys).unwrap().unwrap().peer_id(),
            first.peer_id()
        );
    }

    #[cfg(unix)]
    #[test]
    fn seed_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let _keystore = Keystore::create(keys.clone()).unwrap();
        assert_eq!(
            fs::metadata(keys.join(SEED_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&keys).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn seed_publication_never_overwrites_existing_seed() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let path = keys.join(SEED_FILE);
        let first = [1u8; 32];
        let second = [2u8; 32];
        write_seed_bytes(&path, &first).unwrap();
        let error = write_seed_bytes(&path, &second).unwrap_err();
        assert!(error.to_string().contains("overwrite"), "{error}");
        assert_eq!(fs::read(path).unwrap(), first);
    }

    #[test]
    fn open_rejects_unexpected_files() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let _keystore = Keystore::create(keys.clone()).unwrap();

        fs::write(keys.join("stale.seed"), b"x").unwrap();
        let error = Keystore::open(keys).unwrap_err();
        assert!(
            error.to_string().contains("unexpected keystore entry"),
            "{error}"
        );
    }

    #[test]
    fn open_rejects_interrupted_seed_publication() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        write_private(&keys.join(format!(".{SEED_FILE}.tmp.interrupted")), &[]);
        let error = Keystore::open(keys).unwrap_err();
        assert!(error.to_string().contains("interrupted"), "{error}");
    }

    #[test]
    fn open_rejects_zero_and_malformed_seeds() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");

        assert!(write_seed_bytes(&keys.join(SEED_FILE), &[0u8; 32]).is_err());
        write_private(&keys.join(SEED_FILE), &[0u8; 32]);
        let error = Keystore::open(keys.clone()).unwrap_err();
        assert!(format!("{error:#}").contains("all-zero"), "{error:#}");

        fs::remove_file(keys.join(SEED_FILE)).unwrap();
        write_private(&keys.join(SEED_FILE), &[1u8; 31]);
        let error = Keystore::open(keys).unwrap_err();
        assert!(error.to_string().contains("32 bytes"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_symlinked_or_insecure_seed() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let _keystore = Keystore::create(keys.clone()).unwrap();
        let seed_path = keys.join(SEED_FILE);
        let seed: [u8; 32] = fs::read(&seed_path).unwrap().try_into().unwrap();

        let outside = dir.path().join("outside");
        fs::write(&outside, seed).unwrap();
        fs::remove_file(&seed_path).unwrap();
        symlink(&outside, &seed_path).unwrap();
        assert!(Keystore::open(keys.clone()).is_err());

        fs::remove_file(&seed_path).unwrap();
        write_seed_bytes(&seed_path, &seed).unwrap();
        let mut permissions = fs::metadata(&seed_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&seed_path, permissions).unwrap();
        assert!(Keystore::open(keys).is_err());
    }

    #[test]
    fn open_rejects_non_regular_seed_path() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        fs::create_dir(keys.join(SEED_FILE)).unwrap();
        assert!(Keystore::open(keys).is_err());
    }
}
