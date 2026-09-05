//! The keystore: many identities, one active, all custody-side.
//!
//! Each identity is one 32-byte [`SecretKey`] in `keys/<peer_hex>.seed`, mode
//! `0600`. Labels, the active pointer, and the complete identity set live in
//! `keys/index.json` (also `0600`), so everything sensitive sits in one
//! owner-only directory. Seeds never leave the daemon: callers get only public
//! material ([`IdInfo`]) or, internally, a built [`NodeKeys`] for signing.
//!
//! Authoritative files are never truncated in place. New seeds are published
//! with a non-overwriting link from a synced private temporary file. Index
//! replacements use a synced private temporary file followed by an atomic
//! rename and parent-directory sync. An interrupted publication is rejected on
//! the next open instead of being guessed at.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, anyhow};
use arena0_api::{IdInfo, IdRef};
use arena0_crypto::{NodeKeys, SecretKey};
use arena0_protocol::{PeerId, PeerIdSource};
use rand::RngCore as _;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const INDEX_FILE: &str = "index.json";
const SEED_EXT: &str = "seed";
const SEED_SUFFIX: &str = ".seed";
const PRIVATE_FILE_MODE: u32 = 0o600;
const PRIVATE_DIR_MODE: u32 = 0o700;

// Temporary names are deliberately recognizable so open() can reject an
// interrupted publication rather than silently ignoring a possibly newer
// identity/index state.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Errors that can cross the identity-management API boundary.
///
/// Only the expected user outcomes have dedicated variants. Filesystem,
/// integrity, and publication failures stay wrapped as [`Storage`](Self::Storage)
/// so the API layer cannot mistake a persistence failure for an invalid
/// identity reference.
#[derive(Debug, thiserror::Error)]
pub enum KeystoreError {
    #[error("no identity matches {reference:?}")]
    NotFound { reference: String },
    #[error("identity reference '{reference}' is ambiguous: {matches} identities match")]
    Ambiguous { reference: String, matches: usize },
    #[error("invalid identity label: {0}")]
    InvalidLabel(String),
    #[error(
        "cannot remove the active Host identity; rotate it through a lifecycle-aware operation"
    )]
    ActiveIdentityRemoval,
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

/// Labels, the active-identity pointer, and the complete identity set,
/// persisted as `index.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    /// Active identity, by `PeerId` hex.
    active: Option<String>,
    /// `PeerId` hex -> operator label.
    #[serde(deserialize_with = "deserialize_unique_labels")]
    labels: BTreeMap<String, String>,
    /// Every identity whose seed is owned by this keystore.
    #[serde(deserialize_with = "deserialize_unique_identities")]
    identities: BTreeSet<String>,
}

fn deserialize_unique_identities<'de, D>(deserializer: D) -> Result<BTreeSet<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    let mut identities = BTreeSet::new();
    for identity in values {
        if !identities.insert(identity) {
            return Err(serde::de::Error::custom(
                "keystore index contains duplicate identities",
            ));
        }
    }
    Ok(identities)
}

fn deserialize_unique_labels<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct LabelsVisitor;

    impl<'de> serde::de::Visitor<'de> for LabelsVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of identity peer ids to labels")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut labels = BTreeMap::new();
            while let Some((peer_hex, label)) = map.next_entry::<String, String>()? {
                if labels.insert(peer_hex, label).is_some() {
                    return Err(serde::de::Error::custom(
                        "keystore index contains duplicate labels",
                    ));
                }
            }
            Ok(labels)
        }
    }

    deserializer.deserialize_map(LabelsVisitor)
}

/// The daemon's identity keystore.
#[derive(Debug)]
pub struct Keystore {
    keys_dir: PathBuf,
    index: Mutex<Index>,
}

impl Keystore {
    /// Open the keystore rooted at `keys_dir`, loading and reconciling the
    /// index if present.
    pub fn open(keys_dir: PathBuf) -> anyhow::Result<Self> {
        crate::paths::validate_path(&keys_dir).context("validate keystore path")?;
        validate_keys_dir(&keys_dir)?;

        let index = load_index(&keys_dir)?;
        let seed_ids = scan_seed_files(&keys_dir)?;
        reconcile_index(&index, &seed_ids)?;

        Ok(Self {
            keys_dir,
            index: Mutex::new(index),
        })
    }

    /// Generate a new identity, persist its seed (mode 0600), and make it active if
    /// it is the first. Returns only public material.
    pub fn new_identity(&self, label: Option<String>) -> Result<IdInfo, KeystoreError> {
        if let Some(label) = &label {
            crate::store::validate_plain_name(label).map_err(KeystoreError::InvalidLabel)?;
        }

        // Serializing all seed/index transitions through one lock keeps the
        // proposed index and its seed publication in one local transaction.
        let mut index = self.index.lock().unwrap();
        self.reconcile_current(&index)?;
        let seed = generate_seed()?;
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes(*seed));
        let peer_id = crypto.peer_id();
        let peer_hex = hex::encode(peer_id.0);

        if index.identities.contains(&peer_hex) {
            return Err(KeystoreError::Storage(anyhow!(
                "generated identity collides with existing peer id"
            )));
        }

        let seed_path = self.seed_path(&peer_hex);
        write_seed_bytes(&seed_path, &seed)?;

        let mut proposed = index.clone();
        proposed.identities.insert(peer_hex.clone());
        if let Some(label) = &label {
            proposed.labels.insert(peer_hex.clone(), label.clone());
        }
        if proposed.active.is_none() {
            proposed.active = Some(peer_hex.clone());
        }

        if let Err(index_error) = self.persist(&proposed) {
            // A process crash in this window is intentionally detected as an
            // orphan seed by open(). In the ordinary error path, remove the
            // unpublished identity so callers do not observe a stale orphan.
            return match remove_seed_file(&seed_path) {
                Ok(()) => Err(index_error.into()),
                Err(cleanup_error) => Err(KeystoreError::Storage(anyhow!(
                    "persist keystore index: {index_error}; remove orphan seed: {cleanup_error}"
                ))),
            };
        }

        *index = proposed;
        let active = index.active.as_deref() == Some(peer_hex.as_str());
        drop(index);

        Ok(info_from_crypto(&crypto, peer_id, label, active)?)
    }

    /// List every identity with its public material.
    pub fn list(&self) -> Result<Vec<IdInfo>, KeystoreError> {
        let index = self.index.lock().unwrap();
        self.reconcile_current(&index)?;

        let mut out = Vec::with_capacity(index.identities.len());
        for peer_hex in &index.identities {
            let info = self
                .info_for(peer_hex, &index)?
                .ok_or_else(|| anyhow!("identity seed missing for {peer_hex}"))?;
            out.push(info);
        }
        out.sort_by_key(|i| i.peer_id.0);
        Ok(out)
    }

    /// Look up one identity's public material.
    pub fn show(&self, id: &IdRef) -> Result<IdInfo, KeystoreError> {
        let index = self.index.lock().unwrap();
        self.reconcile_current(&index)?;
        let peer_hex = self.resolve_hex(id, &index)?;
        self.info_for(&peer_hex, &index)?
            .ok_or_else(|| KeystoreError::Storage(anyhow!("identity seed missing for {peer_hex}")))
    }

    /// Set the active identity. Errors if the identity is unknown.
    pub fn set_active(&self, id: &IdRef) -> Result<(), KeystoreError> {
        let mut index = self.index.lock().unwrap();
        self.reconcile_current(&index)?;
        let peer_hex = self.resolve_hex(id, &index)?;
        let mut proposed = index.clone();
        proposed.active = Some(peer_hex);
        self.persist(&proposed)?;
        *index = proposed;
        Ok(())
    }

    /// Remove an inactive identity (its seed and label).
    ///
    /// The active identity is the Host's durable protocol identity. Removing it
    /// while the daemon has cached its key would leave the running Host and its
    /// next recovery with different identity authorities, so rotation must use
    /// a separate lifecycle-aware operation.
    pub fn remove(&self, id: &IdRef) -> Result<(), KeystoreError> {
        let mut index = self.index.lock().unwrap();
        self.reconcile_current(&index)?;
        let peer_hex = self.resolve_hex(id, &index)?;
        if index.active.as_deref() == Some(peer_hex.as_str()) {
            return Err(KeystoreError::ActiveIdentityRemoval);
        }
        let seed_path = self.seed_path(&peer_hex);

        let mut proposed = index.clone();
        proposed.identities.remove(&peer_hex);
        proposed.labels.remove(&peer_hex);
        if proposed.active.as_deref() == Some(peer_hex.as_str()) {
            proposed.active = None;
        }

        // Publish the index first. If the process stops before the seed is
        // removed, open() sees the still-canonical seed as an orphan and
        // refuses to choose an identity implicitly.
        self.persist(&proposed)?;
        *index = proposed;
        drop(index);

        Ok(remove_seed_file(&seed_path)?)
    }

    /// The active identity's `PeerId`, if one is set.
    pub fn active_peer_id(&self) -> Option<PeerId> {
        let index = self.index.lock().unwrap();
        index.active.as_deref().and_then(parse_peer_hex)
    }

    /// Build [`NodeKeys`] for the active identity (custody-side signing).
    pub fn active_crypto(&self) -> anyhow::Result<NodeKeys> {
        let index = self.index.lock().unwrap();
        self.reconcile_current(&index)?;
        let active = index
            .active
            .clone()
            .ok_or_else(|| anyhow!("no active identity; run `arena0 identity new`"))?;
        drop(index);
        self.load_crypto(&active)?
            .ok_or_else(|| anyhow!("active identity seed missing"))
    }

    fn seed_path(&self, peer_hex: &str) -> PathBuf {
        debug_assert!(is_canonical_peer_hex(peer_hex));
        self.keys_dir.join(format!("{peer_hex}.{SEED_EXT}"))
    }

    fn load_crypto(&self, peer_hex: &str) -> anyhow::Result<Option<NodeKeys>> {
        validate_peer_hex(peer_hex)?;
        let path = self.seed_path(peer_hex);
        match fs::symlink_metadata(&path) {
            Ok(_) => load_crypto_from_path(&path, peer_hex).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("inspect seed file {}", path.display()))
            }
        }
    }

    fn info_for(&self, peer_hex: &str, index: &Index) -> anyhow::Result<Option<IdInfo>> {
        let Some(crypto) = self.load_crypto(peer_hex)? else {
            return Ok(None);
        };
        let peer_id = crypto.peer_id();
        let label = index.labels.get(peer_hex).cloned();
        let active = index.active.as_deref() == Some(peer_hex);
        Ok(Some(info_from_crypto(&crypto, peer_id, label, active)?))
    }

    /// Resolve an [`IdRef`] to a `PeerId` hex string against the index. Full peer
    /// ids and exact labels resolve as before; an 8+ hex-char prefix resolves
    /// against verified identity seed files, returning typed reference errors.
    fn resolve_hex(&self, id: &IdRef, index: &Index) -> Result<String, KeystoreError> {
        match id {
            IdRef::Peer(p) => {
                let hex = hex::encode(p.0);
                if !index.identities.contains(&hex) {
                    return Err(KeystoreError::NotFound {
                        reference: Self::identity_reference(id),
                    });
                }
                self.load_crypto(&hex)?.ok_or_else(|| {
                    KeystoreError::Storage(anyhow!("identity seed missing for {hex}"))
                })?;
                Ok(hex)
            }
            IdRef::Label(label) => {
                // Exact label match — like program names, duplicate labels must be ambiguous, not first-match.
                let label_matches: Vec<String> = index
                    .labels
                    .iter()
                    .filter(|(_, v)| *v == label)
                    .map(|(k, _)| k.clone())
                    .collect();
                match label_matches.len() {
                    0 => {}
                    1 => return Ok(label_matches.into_iter().next().unwrap()),
                    n => {
                        return Err(KeystoreError::Ambiguous {
                            reference: label.clone(),
                            matches: n,
                        });
                    }
                }
                let prefix = label.to_ascii_lowercase();
                if prefix.len() >= 8 && prefix.chars().all(|c| c.is_ascii_hexdigit()) {
                    let matches: Vec<String> = self
                        .known_peer_hexes()?
                        .into_iter()
                        .filter(|h| index.identities.contains(h) && h.starts_with(&prefix))
                        .collect();
                    return match matches.len() {
                        0 => Err(KeystoreError::NotFound {
                            reference: label.clone(),
                        }),
                        1 => Ok(matches.into_iter().next().unwrap()),
                        n => Err(KeystoreError::Ambiguous {
                            reference: label.clone(),
                            matches: n,
                        }),
                    };
                }
                Err(KeystoreError::NotFound {
                    reference: label.clone(),
                })
            }
        }
    }

    fn identity_reference(id: &IdRef) -> String {
        match id {
            IdRef::Peer(peer) => peer.to_string(),
            IdRef::Label(label) => label.clone(),
        }
    }

    /// The peer-hex stems of every verified identity seed file on disk.
    fn known_peer_hexes(&self) -> anyhow::Result<Vec<String>> {
        Ok(scan_seed_files(&self.keys_dir)?.into_iter().collect())
    }

    fn reconcile_current(&self, index: &Index) -> anyhow::Result<()> {
        validate_current_index(&self.keys_dir, index)?;
        let seed_ids = scan_seed_files(&self.keys_dir)?;
        reconcile_index(index, &seed_ids)
    }

    fn persist(&self, index: &Index) -> anyhow::Result<()> {
        validate_index_shape(index)?;
        let bytes = serde_json::to_vec_pretty(index).context("encode keystore index")?;
        let path = self.keys_dir.join(INDEX_FILE);
        atomic_replace_private(&path, &bytes)
    }
}

fn info_from_crypto(
    crypto: &NodeKeys,
    peer_id: PeerId,
    label: Option<String>,
    active: bool,
) -> anyhow::Result<IdInfo> {
    Ok(IdInfo {
        peer_id,
        label,
        transport_key: crypto.ed25519_public_key(),
        active,
    })
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

fn parse_peer_hex(hexs: &str) -> Option<PeerId> {
    decode_peer_hex(hexs).ok().map(PeerId)
}

fn is_canonical_peer_hex(hexs: &str) -> bool {
    hexs.len() == 64
        && hexs
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_peer_hex(hexs: &str) -> anyhow::Result<[u8; 32]> {
    anyhow::ensure!(
        is_canonical_peer_hex(hexs),
        "identity filename/index key must be exactly 64 lowercase hexadecimal characters"
    );
    let bytes = hex::decode(hexs).context("decode identity peer id")?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("identity peer id is not 32 bytes"))
}

fn decode_peer_hex(hexs: &str) -> anyhow::Result<[u8; 32]> {
    validate_peer_hex(hexs)
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

fn load_index(keys_dir: &Path) -> anyhow::Result<Index> {
    let path = keys_dir.join(INDEX_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    ensure_private_regular(&path, &metadata, "keystore index")?;
    let bytes =
        fs::read(&path).with_context(|| format!("read keystore index {}", path.display()))?;
    serde_json::from_slice(&bytes).context("parse keystore index")
}

fn scan_seed_files(keys_dir: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut peer_hexes = BTreeSet::new();
    let mut entries = fs::read_dir(keys_dir)
        .context("read keys directory")?
        .collect::<Result<Vec<_>, _>>()
        .context("read keys directory entry")?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(INDEX_FILE) {
            continue;
        }
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("keystore entry name is not valid UTF-8"))?;
        let Some(peer_hex) = name.strip_suffix(SEED_SUFFIX) else {
            anyhow::bail!("unexpected keystore entry {name:?}; interrupted or unsupported state");
        };
        validate_peer_hex(peer_hex).with_context(|| format!("validate seed filename {name:?}"))?;
        let path = entry.path();
        load_crypto_from_path(&path, peer_hex)
            .with_context(|| format!("validate seed filename/content {name:?}"))?;
        if !peer_hexes.insert(peer_hex.to_string()) {
            anyhow::bail!("duplicate identity seed for peer id {peer_hex}");
        }
    }
    Ok(peer_hexes)
}

fn reconcile_index(index: &Index, seed_ids: &BTreeSet<String>) -> anyhow::Result<()> {
    validate_index_shape(index)?;

    if let Some(active) = &index.active {
        anyhow::ensure!(
            seed_ids.contains(active),
            "active identity seed missing for {active}"
        );
    }
    if let Some(missing) = index.identities.difference(seed_ids).next() {
        anyhow::bail!("keystore index references missing seed for {missing}");
    }
    if let Some(orphan) = seed_ids.difference(&index.identities).next() {
        anyhow::bail!("orphan identity seed is not referenced by keystore index: {orphan}");
    }
    Ok(())
}

fn validate_current_index(keys_dir: &Path, index: &Index) -> anyhow::Result<()> {
    let path = keys_dir.join(INDEX_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure_private_regular(&path, &metadata, "keystore index"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(
                index.identities.is_empty() && index.active.is_none() && index.labels.is_empty(),
                "keystore index is missing while identities are present"
            );
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn validate_index_shape(index: &Index) -> anyhow::Result<()> {
    for peer_hex in &index.identities {
        validate_peer_hex(peer_hex).context("validate identity in keystore index")?;
    }
    if let Some(active) = &index.active {
        validate_peer_hex(active).context("validate active identity in keystore index")?;
        anyhow::ensure!(
            index.identities.contains(active),
            "active identity {active} is not in the keystore identity set"
        );
    }
    for (peer_hex, label) in &index.labels {
        validate_peer_hex(peer_hex).context("validate identity label key in keystore index")?;
        anyhow::ensure!(
            index.identities.contains(peer_hex),
            "identity label {peer_hex} is not in the keystore identity set"
        );
        crate::store::validate_plain_name(label).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

fn load_crypto_from_path(path: &Path, expected_peer_hex: &str) -> anyhow::Result<NodeKeys> {
    validate_peer_hex(expected_peer_hex)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect seed file {}", path.display()))?;
    ensure_private_regular(path, &metadata, "seed file")?;
    let bytes = Zeroizing::new(
        fs::read(path).with_context(|| format!("read seed file {}", path.display()))?,
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
    let crypto = NodeKeys::from_secret(SecretKey::from_bytes(*seed));
    let actual = hex::encode(crypto.peer_id().0);
    anyhow::ensure!(
        actual == expected_peer_hex,
        "seed content does not match its filename identity"
    );
    Ok(crypto)
}

fn ensure_private_regular(path: &Path, metadata: &Metadata, kind: &str) -> anyhow::Result<()> {
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
fn publish_new_private(path: &Path, bytes: &[u8], kind: &str) -> anyhow::Result<()> {
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

/// Replace an existing private index (or create it if absent) through a
/// same-directory temporary file. The authoritative path is never opened with
/// truncate or written in place.
fn atomic_replace_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    crate::paths::validate_path(path).context("validate index path")?;
    ensure_private_file_if_present(path, "keystore index")?;

    let (temp_path, mut temp_file) = create_private_temp(path)?;
    let result = (|| {
        temp_file
            .write_all(bytes)
            .context("write index temporary file")?;
        temp_file.sync_all().context("sync index temporary file")?;
        drop(temp_file);
        fs::rename(&temp_path, path).context("atomically publish keystore index")?;
        sync_parent_dir(path)
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
            Ok(file) => return Ok((temp_path, file)),
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

fn ensure_private_file_if_present(path: &Path, kind: &str) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure_private_regular(path, &metadata, kind),
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

fn remove_seed_file(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect seed file {}", path.display()))?;
    ensure_private_regular(path, &metadata, "seed file")?;
    fs::remove_file(path).with_context(|| format!("remove seed file {}", path.display()))?;
    sync_parent_dir(path)
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

    fn write_index(keys: &Path, index: &Index) {
        let bytes = serde_json::to_vec(index).unwrap();
        let path = keys.join(INDEX_FILE);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(PRIVATE_FILE_MODE);
        }
        let mut file = options.open(path).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn identity(seed_byte: u8) -> (String, [u8; 32]) {
        let seed = [seed_byte; 32];
        let crypto = NodeKeys::from_secret(SecretKey::from_bytes(seed));
        (hex::encode(crypto.peer_id().0), seed)
    }

    #[test]
    fn resolve_hex_rejects_unverified_seed_entries() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        fs::write(keys.join("aaaaaaa11.seed"), b"x").unwrap();
        fs::write(keys.join("aaaaaaa12.seed"), b"x").unwrap();

        let error = scan_seed_files(&keys).unwrap_err();
        assert!(format!("{error:#}").contains("seed filename"), "{error:#}");
    }

    #[test]
    fn new_identity_rejects_control_characters_in_label() {
        let dir = keys_dir();
        let ks = Keystore::open(dir.path().join("keys")).unwrap();
        let err = ks.new_identity(Some("bad\u{1b}label".into())).unwrap_err();
        assert!(err.to_string().contains("control characters"), "{err}");
    }

    #[test]
    fn remove_rejects_the_active_identity() {
        let dir = keys_dir();
        let ks = Keystore::open(dir.path().join("keys")).unwrap();
        let identity = ks.new_identity(Some("host-01".into())).unwrap();

        let error = ks.remove(&IdRef::Peer(identity.peer_id)).unwrap_err();

        assert!(
            error.to_string().contains("active Host identity"),
            "{error}"
        );
        assert_eq!(ks.active_peer_id(), Some(identity.peer_id));
        assert_eq!(ks.list().unwrap().len(), 1);
    }

    #[test]
    fn seed_publication_never_overwrites_existing_seed() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let _keystore = Keystore::open(keys.clone()).unwrap();
        let path = keys.join("seed");
        let first = [1u8; 32];
        let second = [2u8; 32];
        write_seed_bytes(&path, &first).unwrap();
        let error = write_seed_bytes(&path, &second).unwrap_err();
        assert!(error.to_string().contains("overwrite"), "{error}");
        assert_eq!(fs::read(path).unwrap(), first);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(keys).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn open_rejects_orphan_seed_and_missing_active_seed() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let (peer_hex, seed) = identity(7);
        write_seed_bytes(&keys.join(format!("{peer_hex}.{SEED_EXT}")), &seed).unwrap();
        let error = Keystore::open(keys.clone()).unwrap_err();
        assert!(
            error.to_string().contains("orphan identity seed"),
            "{error}"
        );

        fs::remove_file(keys.join(format!("{peer_hex}.{SEED_EXT}"))).unwrap();
        let mut index = Index::default();
        index.identities.insert(peer_hex.clone());
        index.active = Some(peer_hex);
        write_index(&keys, &index);
        let error = Keystore::open(keys).unwrap_err();
        assert!(
            error.to_string().contains("active identity seed missing"),
            "{error}"
        );
    }

    #[test]
    fn open_rejects_seed_identity_collision_and_zero_secret() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let (peer_hex, _) = identity(8);
        let mut index = Index::default();
        index.identities.insert(peer_hex.clone());
        write_index(&keys, &index);

        let wrong = identity(9).1;
        write_seed_bytes(&keys.join(format!("{peer_hex}.{SEED_EXT}")), &wrong).unwrap();
        let error = Keystore::open(keys.clone()).unwrap_err();
        assert!(
            error.to_string().contains("content") || error.to_string().contains("derives"),
            "{error}"
        );

        fs::remove_file(keys.join(format!("{peer_hex}.{SEED_EXT}"))).unwrap();
        assert!(
            write_seed_bytes(&keys.join(format!("{peer_hex}.{SEED_EXT}")), &[0u8; 32],).is_err()
        );

        let zero_path = keys.join(format!("{peer_hex}.{SEED_EXT}"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(PRIVATE_FILE_MODE);
        }
        let mut zero_file = options.open(&zero_path).unwrap();
        zero_file.write_all(&[0u8; 32]).unwrap();
        zero_file.sync_all().unwrap();
        drop(zero_file);
        let error = Keystore::open(keys).unwrap_err();
        assert!(format!("{error:#}").contains("all-zero"), "{error:#}");
    }

    #[test]
    fn open_rejects_interrupted_index_publication() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        write_index(&keys, &Index::default());
        let temp = keys.join(".index.json.tmp.interrupted");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(PRIVATE_FILE_MODE);
        }
        options.open(temp).unwrap().sync_all().unwrap();
        let error = Keystore::open(keys).unwrap_err();
        assert!(error.to_string().contains("interrupted"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_symlinked_or_insecure_authoritative_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = keys_dir();
        let keys = dir.path().join("keys");
        let (peer_hex, seed) = identity(10);
        let mut index = Index::default();
        index.identities.insert(peer_hex.clone());
        write_index(&keys, &index);
        let seed_path = keys.join(format!("{peer_hex}.{SEED_EXT}"));
        write_seed_bytes(&seed_path, &seed).unwrap();
        assert!(Keystore::open(keys.clone()).is_ok());

        let outside = dir.path().join("outside");
        fs::write(&outside, fs::read(&seed_path).unwrap()).unwrap();
        fs::remove_file(&seed_path).unwrap();
        symlink(&outside, &seed_path).unwrap();
        assert!(Keystore::open(keys.clone()).is_err());

        fs::remove_file(&seed_path).unwrap();
        write_seed_bytes(&seed_path, &seed).unwrap();
        let index_path = keys.join(INDEX_FILE);
        let mut permissions = fs::metadata(&index_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&index_path, permissions).unwrap();
        assert!(Keystore::open(keys).is_err());
    }

    #[test]
    fn open_rejects_non_regular_index_and_seed_paths() {
        let dir = keys_dir();
        let keys = dir.path().join("keys");
        fs::create_dir(keys.join(INDEX_FILE)).unwrap();
        assert!(Keystore::open(keys.clone()).is_err());

        fs::remove_dir(keys.join(INDEX_FILE)).unwrap();
        let (peer_hex, _) = identity(11);
        let mut index = Index::default();
        index.identities.insert(peer_hex.clone());
        write_index(&keys, &index);
        fs::create_dir(keys.join(format!("{peer_hex}.{SEED_EXT}"))).unwrap();
        assert!(Keystore::open(keys).is_err());
    }
}
