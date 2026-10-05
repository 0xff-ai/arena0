//! Async program-catalog projections backed by the Host's SQLite store.
//!
//! The daemon does not own a second registry.  [`ProgramCatalog`] is a thin
//! read/import boundary over [`arena0_store::StoreHandle`]: program bytes and
//! membership are durable in SQLite, while metadata and schemas are decoded
//! from those same bytes at the API boundary and cached by immutable hash.

use anyhow::Context as _;
use arena0_api::{ProgramDetail, ProgramRefError, ProgramSummary};
use arena0_program::{ProgramHash, ProgramSchema};
use arena0_sandbox::{Program, SandboxError, WasmtimeEngine};
use arena0_store::{ProgramRemoveOutcome, ProgramStoreOutcome, StoreError, StoreHandle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
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

/// One Host's program-catalog projection. Clones share the asynchronous store
/// capability and immutable parsed details; durable membership stays in SQLite.
#[derive(Clone, Debug)]
pub(crate) struct ProgramCatalog {
    store: StoreHandle,
    /// Parsed metadata and schema by content hash. Shared by clones of this
    /// catalog (one per Host service); never invalidated, because a hash names
    /// immutable bytes. Bounded by the Host's registered programs.
    details: Arc<StdMutex<HashMap<ProgramHash, Arc<ProgramDetail>>>>,
    #[cfg(test)]
    detail_parses: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ProgramCatalog {
    /// Bind a projection to one Host-owned store.
    #[must_use]
    pub(crate) fn new(store: StoreHandle) -> Self {
        Self {
            store,
            details: Arc::new(StdMutex::new(HashMap::new())),
            #[cfg(test)]
            detail_parses: Default::default(),
        }
    }

    /// List active program summaries, decoding metadata from durable bytes.
    pub(crate) async fn list(&self) -> anyhow::Result<Vec<ProgramSummary>> {
        let hashes = self.store.list_programs(PROGRAM_LIST_LIMIT).await?;
        let mut programs = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let detail = self
                .detail(hash)
                .await?
                .ok_or_else(|| anyhow::anyhow!("program {hash} is not registered"))?;
            programs.push(detail.summary);
        }
        programs.sort_by_key(|program| program.program_hash.0);
        Ok(programs)
    }

    /// Resolve a full id, short hash prefix, or unique program name.
    pub(crate) async fn resolve(
        &self,
        reference: &str,
    ) -> anyhow::Result<Result<ProgramHash, ProgramRefError>> {
        if reference.len() == 64
            && let Ok(id) = reference.parse::<ProgramHash>()
        {
            let hashes = self.store.list_programs(PROGRAM_LIST_LIMIT).await?;
            return Ok(if hashes.contains(&id) {
                Ok(id)
            } else {
                Err(ProgramRefError::NotFound {
                    reference: reference.to_owned(),
                })
            });
        }

        let programs = self.list().await?;

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
        // Always consult the store, including on hits: load_program retains
        // removed artifacts and validates their durable bytes. The lock covers
        // parsing so concurrent cold readers cannot parse the same hash twice;
        // no asynchronous work runs while it is held.
        let mut details = self.details.lock().expect("program detail cache poisoned");
        if let Some(detail) = details.get(&hash) {
            return Ok(Some(detail.as_ref().clone()));
        }
        #[cfg(test)]
        self.detail_parses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let program = Program::try_from(stored.wasm().to_vec())
            .with_context(|| format!("parse registered program {hash}"))?;
        let detail = ProgramDetail {
            summary: summary(&program),
            schema: program.definition().schema.clone(),
        };
        details.insert(hash, Arc::new(detail.clone()));
        Ok(Some(detail))
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
    use arena0_test_engine::shared_test_engine;
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
    async fn catalog_details_parse_once_across_reads_and_clones() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(4, &directory);
        let catalog = ProgramCatalog::new(store.handle().clone());
        let clone = catalog.clone();
        let (hash, _) = catalog
            .import(
                crate::assets::PROGRAMS[0].to_vec(),
                &shared_test_engine(),
                1,
            )
            .await
            .unwrap();

        // Concurrent cold reads must share the parse as well as subsequent hits.
        let (detail, schema) = tokio::join!(catalog.detail(hash), clone.schema(hash));
        let detail = detail.unwrap().unwrap();
        assert_eq!(schema.unwrap(), Some(detail.schema.clone()));
        assert_eq!(clone.detail(hash).await.unwrap(), Some(detail.clone()));
        assert_eq!(catalog.schema(hash).await.unwrap(), Some(detail.schema));
        assert_eq!(clone.list().await.unwrap(), vec![detail.summary.clone()]);
        assert_eq!(catalog.list().await.unwrap(), vec![detail.summary]);
        assert_eq!(
            catalog
                .detail_parses
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        store.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn catalog_exact_hash_resolve_does_not_parse_details() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(5, &directory);
        let catalog = ProgramCatalog::new(store.handle().clone());
        let (hash, _) = catalog
            .import(
                crate::assets::PROGRAMS[0].to_vec(),
                &shared_test_engine(),
                1,
            )
            .await
            .unwrap();
        assert_eq!(catalog.resolve(&hash.to_string()).await.unwrap(), Ok(hash));
        assert_eq!(
            catalog
                .detail_parses
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        store.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn catalog_removed_programs_retain_details_but_leave_active_results() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(6, &directory);
        let catalog = ProgramCatalog::new(store.handle().clone());
        let (hash, _) = catalog
            .import(
                crate::assets::PROGRAMS[0].to_vec(),
                &shared_test_engine(),
                1,
            )
            .await
            .unwrap();
        let before = catalog.detail(hash).await.unwrap().unwrap();
        assert_eq!(
            catalog.remove(hash, 2).await.unwrap(),
            ProgramRemoveOutcome::Removed
        );
        assert!(catalog.list().await.unwrap().is_empty());
        // Removal hides active membership but retains artifacts for executions.
        for projection in [catalog.clone(), ProgramCatalog::new(store.handle().clone())] {
            assert_eq!(projection.detail(hash).await.unwrap(), Some(before.clone()));
            assert_eq!(
                projection.schema(hash).await.unwrap(),
                Some(before.schema.clone())
            );
            assert_eq!(
                projection.load_program(hash).await.unwrap().bytes(),
                crate::assets::PROGRAMS[0]
            );
            for reference in [
                hash.to_string(),
                hash.to_string()[..8].to_owned(),
                before.summary.name.clone(),
            ] {
                assert_eq!(
                    projection.resolve(&reference).await.unwrap(),
                    Err(ProgramRefError::NotFound { reference })
                );
            }
        }
        store.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn catalogs_are_isolated_per_host() {
        let first_dir = tempfile::tempdir().expect("first directory");
        let second_dir = tempfile::tempdir().expect("second directory");
        let first = store(1, &first_dir);
        let second = store(2, &second_dir);
        let first_catalog = ProgramCatalog::new(first.handle().clone());
        let second_catalog = ProgramCatalog::new(second.handle().clone());
        let engine = shared_test_engine();
        let (hash, _) = first_catalog
            .import(crate::assets::PROGRAMS[0].to_vec(), &engine, 1)
            .await
            .expect("register first program");

        let programs = first_catalog.list().await.unwrap();
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].program_hash, hash);
        assert_eq!(programs[0].name, "rock-paper-scissors");
        assert_eq!(
            first_catalog.resolve("rock-paper-scissors").await.unwrap(),
            Ok(hash)
        );
        assert!(second_catalog.list().await.unwrap().is_empty());
        assert!(second_catalog.detail(hash).await.unwrap().is_none());
        assert!(second_catalog.load_program(hash).await.is_err());
        assert!(matches!(
            second_catalog.resolve("rock-paper-scissors").await.unwrap(),
            Err(ProgramRefError::NotFound { .. })
        ));
        assert!(matches!(
            second_catalog.import(vec![1, 2, 3], &engine, 2).await,
            Err(CatalogError::InvalidProgram(_))
        ));
        assert!(second_catalog.list().await.unwrap().is_empty());
        first.shutdown().await.expect("shutdown first store");
        second.shutdown().await.expect("shutdown second store");
    }

    #[tokio::test]
    async fn catalog_membership_and_bytes_survive_reopen() {
        let directory = tempfile::tempdir().expect("directory");
        let first = store(3, &directory);
        let catalog = ProgramCatalog::new(first.handle().clone());
        assert!(catalog.list().await.unwrap().is_empty());
        let engine = shared_test_engine();
        let wasm = crate::assets::PROGRAMS[0];
        let (hash, _) = catalog
            .import(wasm.to_vec(), &engine, 2)
            .await
            .expect("register program");
        let before = catalog.detail(hash).await.unwrap().unwrap();
        assert_eq!(before.summary.name, "rock-paper-scissors");
        assert!(!before.schema.callouts.is_empty());
        first.shutdown().await.expect("shutdown store");

        let reopened = store(3, &directory);
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
    }
}
