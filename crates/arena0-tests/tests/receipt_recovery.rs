//! Crash cuts before and after atomic stop-report publication.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arena0_node::{ExecContext, Host, SessionMessage};
use arena0_program::JsonBytes;
use arena0_protocol::{
    AbortKind, AbortOccurrence, ExecId, ExecutionAdmission, ExecutionInput, NegotiationId,
    PeerIdSource, StateHash,
};
use arena0_sandbox::{InitializeCall, Program, WasmtimeEngine};
use arena0_store::{RecoveryCursor, Store, StoreConfig};
use arena0_tests::fixtures::{activation_for, execution_key, ordering_program_wasm, provider};
use arena0_transport::local::{LocalNetwork, LocalTransport};

#[derive(Clone, Copy, Debug)]
enum CrashAfter {
    Stop,
    Publication,
}

#[derive(Debug)]
enum Milestone {
    Started,
    CrashBoundaryPersisted,
    RecoveryStarted,
    AbortObserved,
    OutboxDrained,
    ReceiptReplayed,
    SecondRestartVerified,
}

async fn recover_after(cut: CrashAfter) {
    let started = Instant::now();
    let mut progress = vec![(Milestone::Started, started.elapsed())];
    tokio::time::timeout(Duration::from_secs(15), async {
        let directory = tempfile::tempdir().expect("temporary home");
        let path = directory.path().join("arena0.sqlite");
        let keys = [provider(7), provider(8)];
        let peers = keys.iter().map(PeerIdSource::peer_id).collect::<Vec<_>>();
        let wasm = ordering_program_wasm(false);
        let program = Program::try_from(wasm.clone()).expect("program");
        let admitted = WasmtimeEngine::new().unwrap().admit(&program).unwrap();
        let params = JsonBytes::try_new(br#"{}"#.to_vec()).unwrap();
        let initialized = admitted
            .initialize(InitializeCall::new(params.clone()))
            .unwrap();
        let exec_id = ExecId([0x73; 32]);
        let negotiation_id = NegotiationId([0x74; 32]);
        let activation = activation_for(
            &keys,
            negotiation_id,
            program.hash(),
            params.as_bytes().to_vec(),
            StateHash::of(initialized.shared.as_bytes()),
        );
        let store = Store::open(StoreConfig::new(&path, peers[0])).unwrap();
        store
            .handle()
            .register_program(wasm.clone(), 1)
            .await
            .unwrap();
        let mut writer = store.handle().claim_execution(exec_id).unwrap();
        writer
            .create_execution_request(
                program.hash(),
                Some(params.clone()),
                ExecutionAdmission::explicit(negotiation_id, peers.clone()).unwrap(),
                2,
            )
            .await
            .unwrap();
        writer
            .prepare_activation(activation.prepared().clone(), 3)
            .await
            .unwrap();
        writer
            .commit_activation(activation.clone(), 4)
            .await
            .unwrap();
        writer
            .create_execution(
                activation.clone(),
                peers[0],
                initialized.shared,
                initialized.local,
                5,
            )
            .await
            .unwrap();
        writer
            .apply_input(ExecutionInput::Activate, 6)
            .await
            .unwrap();
        let state = writer.load_execution().await.unwrap().unwrap();
        let unsigned = AbortOccurrence::unsigned(
            activation.session_hash(),
            peers[1],
            AbortKind::Abort,
            7,
            "peer stopped before the first public call",
            state.public(),
        )
        .unwrap();
        let signature = keys[1].sign(&unsigned.signing_bytes().unwrap());
        writer
            .apply_input(
                ExecutionInput::Abort(unsigned.with_signature(signature).unwrap()),
                7,
            )
            .await
            .unwrap();
        if matches!(cut, CrashAfter::Publication) {
            writer.assemble_receipt(8).await.unwrap();
        }
        // No actor runs between these durable mutations and the store close.
        // The reopened Host sees exactly the selected crash boundary.
        drop(writer);
        store.shutdown().await.unwrap();

        progress.push((Milestone::CrashBoundaryPersisted, started.elapsed()));
        let store = Store::open(StoreConfig::new(&path, peers[0])).unwrap();
        let candidates = store
            .handle()
            .list_recovery_candidates(RecoveryCursor::start(), 10)
            .await
            .unwrap();
        assert_eq!(
            candidates.candidates().len(),
            1,
            "lost recovery work at {cut:?}"
        );
        let network = LocalNetwork::new();
        let mut transports = LocalTransport::create_network(&network, peers.clone())
            .expect("attach local transports");
        let transport = Arc::new(transports.remove(0));
        let identity = Arc::new(provider(7));
        let host = Host::start(Arc::clone(&identity), transport, store.handle().clone());
        let writer = host.claim_execution(exec_id).unwrap();
        let context = ExecContext::new(
            exec_id,
            admitted,
            params,
            activation,
            execution_key(&identity),
        );
        let mut spawned = host.spawn(context, writer).unwrap();
        progress.push((Milestone::RecoveryStarted, started.elapsed()));
        loop {
            match spawned.message_rx.recv().await.expect("actor observation") {
                SessionMessage::Aborted { .. } => break,
                SessionMessage::Failed { reason } => panic!("recovery failed at {cut:?}: {reason}"),
                _ => {}
            }
        }
        progress.push((Milestone::AbortObserved, started.elapsed()));
        loop {
            if store
                .handle()
                .list_recovery_candidates(RecoveryCursor::start(), 10)
                .await
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        progress.push((Milestone::OutboxDrained, started.elapsed()));
        let receipts = store.handle().list_receipts(10).await.unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].provenance,
            arena0_store::ReceiptProvenance::Produced
        );
        assert!(matches!(
            receipts[0].receipt,
            arena0_protocol::ReceiptArtifact::StopReport(_)
        ));
        let verified =
            arena0_verify::verify_full(&wasm, &receipts[0].receipt.encode().unwrap()).unwrap();
        assert!(matches!(
            verified.terminal,
            arena0_verify::VerifiedTerminal::Stopped { .. }
        ));
        progress.push((Milestone::ReceiptReplayed, started.elapsed()));
        spawned.shutdown().await;
        host.stop().await;
        store.shutdown().await.unwrap();
        let reopened = Store::open(StoreConfig::new(&path, peers[0])).unwrap();
        assert_eq!(reopened.handle().list_receipts(10).await.unwrap(), receipts);
        assert!(
            reopened
                .handle()
                .list_recovery_candidates(RecoveryCursor::start(), 10)
                .await
                .unwrap()
                .is_empty()
        );
        reopened.shutdown().await.unwrap();
        progress.push((Milestone::SecondRestartVerified, started.elapsed()));
    })
    .await
    .unwrap_or_else(|_| {
        panic!("receipt recovery exceeded 15 seconds after {cut:?}; milestones: {progress:?}")
    });
}

#[tokio::test]
async fn stopped_execution_recovers_before_receipt_assembly() {
    recover_after(CrashAfter::Stop).await;
}

#[tokio::test]
async fn stopped_execution_recovers_after_atomic_publication() {
    recover_after(CrashAfter::Publication).await;
}
