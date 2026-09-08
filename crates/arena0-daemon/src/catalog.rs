//! Async program-catalog projections backed by the Host's SQLite store.
//!
//! The daemon does not own a second registry.  [`ProgramCatalog`] is a thin
//! read/import boundary over [`arena0_store::StoreHandle`]: program bytes and
//! membership are durable in SQLite, while metadata and schemas are decoded
//! from those same bytes at the API boundary.

use anyhow::Context as _;
use arena0_api::{ProgramDetail, ProgramRefError, ProgramSummary};
use arena0_program::{ProgramHash, ProgramSchema};
use arena0_sandbox::{Program, SandboxError, WasmtimeEngine};
use arena0_store::{ProgramRemoveOutcome, ProgramStoreOutcome, StoreError, StoreHandle};
use thiserror::Error;

/// Maximum catalog rows materialized by one API projection.
const PROGRAM_LIST_LIMIT: usize = 1_024;

/// Errors at the program-import boundary. A malformed or invalid Wasm artifact
/// is caller input; only the durable registration operation can be a
/// persistence failure.
#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("invalid program: {0}")]
    InvalidProgram(#[from] SandboxError),
    #[error(transparent)]
    Storage(#[from] StoreError),
}

/// One Host's program-catalog projection. Cloning it only clones the bounded
/// asynchronous store capability; it never clones or caches program state.
#[derive(Clone, Debug)]
pub(crate) struct ProgramCatalog {
    store: StoreHandle,
}

impl ProgramCatalog {
    /// Bind a projection to one Host-owned store.
    #[must_use]
    pub(crate) fn new(store: StoreHandle) -> Self {
        Self { store }
    }

    /// List active program summaries, decoding metadata from durable bytes.
    pub(crate) async fn list(&self) -> anyhow::Result<Vec<ProgramSummary>> {
        let hashes = self.store.list_programs(PROGRAM_LIST_LIMIT).await?;
        let mut programs = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let program = self.load_program(hash).await?;
            programs.push(summary(&program));
        }
        programs.sort_by_key(|program| program.program_hash.0);
        Ok(programs)
    }

    /// Resolve a full id, short hash prefix, or unique program name.
    pub(crate) async fn resolve(
        &self,
        reference: &str,
    ) -> anyhow::Result<Result<ProgramHash, ProgramRefError>> {
        let programs = self.list().await?;
        if reference.len() == 64
            && let Ok(id) = reference.parse::<ProgramHash>()
        {
            return Ok(
                if programs.iter().any(|program| program.program_hash == id) {
                    Ok(id)
                } else {
                    Err(ProgramRefError::NotFound {
                        reference: reference.to_owned(),
                    })
                },
            );
        }

        let by_name = programs
            .iter()
            .filter(|program| program.name == reference)
            .map(|program| program.program_hash)
            .collect::<Vec<_>>();
        if !by_name.is_empty() {
            return Ok(match by_name.as_slice() {
                [program] => Ok(*program),
                _ => Err(ProgramRefError::Ambiguous {
                    reference: reference.to_owned(),
                    candidates: by_name,
                }),
            });
        }

        if !reference.is_empty()
            && reference
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            let prefix = reference.to_ascii_lowercase();
            let by_prefix = programs
                .iter()
                .filter(|program| hex::encode(program.program_hash.0).starts_with(&prefix))
                .map(|program| program.program_hash)
                .collect::<Vec<_>>();
            return Ok(match by_prefix.as_slice() {
                [] => Err(ProgramRefError::NotFound {
                    reference: reference.to_owned(),
                }),
                [program] => Ok(*program),
                _ => Err(ProgramRefError::Ambiguous {
                    reference: reference.to_owned(),
                    candidates: by_prefix,
                }),
            });
        }

        Ok(Err(ProgramRefError::NotFound {
            reference: reference.to_owned(),
        }))
    }

    /// Load and parse one exact durable artifact.
    pub(crate) async fn load_program(&self, hash: ProgramHash) -> anyhow::Result<Program> {
        let stored = self
            .store
            .load_program(hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("program {hash} is not registered"))?;
        Program::try_from(stored.wasm().to_vec())
            .with_context(|| format!("parse registered program {hash}"))
    }

    /// Return one program's detail projection from the registered bytes.
    pub(crate) async fn detail(&self, hash: ProgramHash) -> anyhow::Result<Option<ProgramDetail>> {
        let Some(stored) = self.store.load_program(hash).await? else {
            return Ok(None);
        };
        let program = Program::try_from(stored.wasm().to_vec())
            .with_context(|| format!("parse registered program {hash}"))?;
        Ok(Some(ProgramDetail {
            summary: summary(&program),
            schema: program.definition().schema.clone(),
        }))
    }

    /// Return one program's public schema projection.
    pub(crate) async fn schema(&self, hash: ProgramHash) -> anyhow::Result<Option<ProgramSchema>> {
        Ok(self.detail(hash).await?.map(|detail| detail.schema))
    }

    /// Import and durably register one artifact. Validation occurs before
    /// catalog registration; the engine may retain compiled code for later
    /// execution loads, and every execution still receives a fresh guest
    /// instance.
    pub(crate) async fn import(
        &self,
        wasm: Vec<u8>,
        engine: &WasmtimeEngine,
        now_ms: u64,
    ) -> Result<(ProgramHash, ProgramStoreOutcome), CatalogError> {
        let program = Program::try_from(wasm).map_err(CatalogError::InvalidProgram)?;
        let _loaded = engine
            .load(&program)
            .map_err(CatalogError::InvalidProgram)?;
        self.store
            .register_program(program.bytes().to_vec(), now_ms)
            .await
            .map_err(CatalogError::Storage)
    }

    /// Remove a program from the active catalog while retaining bytes needed by
    /// existing durable executions.
    pub(crate) async fn remove(
        &self,
        hash: ProgramHash,
        now_ms: u64,
    ) -> Result<ProgramRemoveOutcome, StoreError> {
        self.store.remove_program(hash, now_ms).await
    }
}

fn summary(program: &Program) -> ProgramSummary {
    let metadata = &program.definition().metadata;
    ProgramSummary {
        program_hash: program.hash(),
        name: metadata.name.clone(),
        display_name: metadata.display_name.clone(),
        version: metadata.version.clone(),
        description: metadata.description.clone(),
        participants: metadata.participants,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::{NodeKeys, SecretKey};
    use arena0_protocol::PeerIdSource;
    use arena0_store::{Store, StoreConfig};
    use tempfile::TempDir;

    fn store(seed: u8, directory: &TempDir) -> Store {
        let peer = NodeKeys::from_secret(SecretKey::from_bytes([seed; 32])).peer_id();
        Store::open(StoreConfig::new(
            directory.path().join("arena0.sqlite"),
            peer,
        ))
        .expect("open store")
    }

    #[tokio::test]
    async fn catalog_membership_and_bytes_survive_reopen() {
        let first_directory = tempfile::tempdir().expect("first directory");
        let second_directory = tempfile::tempdir().expect("second directory");
        let first = store(3, &first_directory);
        let second = store(4, &second_directory);
        let catalog = ProgramCatalog::new(first.handle().clone());
        let other_catalog = ProgramCatalog::new(second.handle().clone());
        assert!(catalog.list().await.unwrap().is_empty());
        assert!(other_catalog.list().await.unwrap().is_empty());
        let engine = WasmtimeEngine::new().expect("engine");
        let wasm = crate::assets::PROGRAMS[0];
        let (hash, _) = catalog
            .import(wasm.to_vec(), &engine, 2)
            .await
            .expect("register program");
        let before = catalog.detail(hash).await.unwrap().unwrap();
        assert_eq!(before.summary.name, "rock-paper-scissors");
        assert!(!before.schema.callouts.is_empty());
        assert_eq!(catalog.list().await.unwrap(), vec![before.summary.clone()]);
        assert_eq!(
            catalog.resolve("rock-paper-scissors").await.unwrap(),
            Ok(hash)
        );
        assert!(other_catalog.list().await.unwrap().is_empty());
        assert!(other_catalog.detail(hash).await.unwrap().is_none());
        assert!(other_catalog.load_program(hash).await.is_err());
        assert!(matches!(
            other_catalog.resolve("rock-paper-scissors").await.unwrap(),
            Err(ProgramRefError::NotFound { .. })
        ));
        assert!(matches!(
            other_catalog.import(vec![1, 2, 3], &engine, 3).await,
            Err(CatalogError::InvalidProgram(_))
        ));
        assert!(other_catalog.list().await.unwrap().is_empty());
        first.shutdown().await.expect("shutdown store");

        let reopened = store(3, &first_directory);
        let catalog = ProgramCatalog::new(reopened.handle().clone());
        assert_eq!(catalog.list().await.unwrap(), vec![before.summary.clone()]);
        assert_eq!(
            catalog.resolve("rock-paper-scissors").await.unwrap(),
            Ok(hash)
        );
        assert_eq!(catalog.detail(hash).await.unwrap(), Some(before.clone()));
        assert_eq!(catalog.schema(hash).await.unwrap(), Some(before.schema));
        assert_eq!(catalog.load_program(hash).await.unwrap().bytes(), wasm);
        reopened.shutdown().await.expect("shutdown reopened store");
        second.shutdown().await.expect("shutdown second store");
    }
}
