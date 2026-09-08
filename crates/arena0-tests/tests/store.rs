//! Public program-catalog projections of the Host-owned SQLite store.

use arena0_crypto::{NodeKeys, SecretKey};
use arena0_program::ProgramHash;
use arena0_protocol::PeerIdSource;
use arena0_sandbox::Program;
use arena0_store::{ProgramStoreOutcome, Store, StoreConfig};
use arena0_tests::wasm::program_wasm;

fn owner() -> NodeKeys {
    NodeKeys::from_secret(SecretKey::from_bytes([0x71; 32]))
}

#[tokio::test]
async fn registry_projection_is_content_addressed_and_durable() {
    let wasm = program_wasm("rock_paper_scissors");
    let program = Program::try_from(wasm.clone()).expect("program metadata");
    let expected = ProgramHash::of(&wasm);
    assert_eq!(program.hash(), expected);

    let directory = tempfile::tempdir().expect("temporary store directory");
    let path = directory.path().join("arena0.sqlite");
    let peer = owner().peer_id();
    let store = Store::open(StoreConfig::new(&path, peer)).expect("open sqlite store");
    let handle = store.handle().clone();

    let (hash, first) = handle
        .register_program(wasm.clone(), 1)
        .await
        .expect("register program");
    assert_eq!(hash, expected);
    assert_eq!(first, ProgramStoreOutcome::Stored);

    let (same_hash, again) = handle
        .register_program(wasm.clone(), 2)
        .await
        .expect("idempotent registration");
    assert_eq!(same_hash, expected);
    assert_eq!(again, ProgramStoreOutcome::AlreadyStored);
    assert_eq!(
        handle.list_programs(8).await.expect("list programs"),
        vec![expected]
    );
    assert_eq!(
        handle
            .load_program(expected)
            .await
            .expect("load program")
            .expect("stored program")
            .wasm(),
        wasm.as_slice()
    );

    assert_eq!(
        handle
            .remove_program(expected, 3)
            .await
            .expect("remove program"),
        arena0_store::ProgramRemoveOutcome::Removed
    );
    assert!(
        handle
            .list_programs(8)
            .await
            .expect("list after remove")
            .is_empty()
    );
    // Removal only removes active catalog membership. Existing bytes remain
    // durable for recovery, which is the store's documented projection.
    assert_eq!(
        handle
            .load_program(expected)
            .await
            .expect("load retained bytes")
            .expect("retained program")
            .wasm(),
        wasm.as_slice()
    );

    drop(handle);
    store.shutdown().await.expect("shut down sqlite store");
    let reopened = Store::open(StoreConfig::new(&path, peer)).expect("reopen sqlite store");
    assert!(
        reopened
            .handle()
            .list_programs(8)
            .await
            .expect("list reopened programs")
            .is_empty()
    );
    assert_eq!(
        reopened
            .handle()
            .load_program(expected)
            .await
            .expect("load reopened bytes")
            .expect("retained bytes after reopen")
            .wasm(),
        wasm.as_slice()
    );
    reopened
        .shutdown()
        .await
        .expect("shut down reopened sqlite store");
}
