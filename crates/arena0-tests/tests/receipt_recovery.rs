//! Crash cuts before and after atomic stop-report publication.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arena0_node::{ExecContext, Host, SessionMessage};
use arena0_program::JsonBytes;
use arena0_protocol::{
    AbortKind, AbortOccurrence, ExecId, ExecLifecycle, ExecutionAdmission, ExecutionVersion,
    NegotiationId, PeerIdSource, StateHash,
};
use arena0_sandbox::{InitializeCall, Program, WasmtimeEngine};
use arena0_store::{RecoveryCursor, Store, StoreConfig};
use arena0_tests::fixtures::{activation_for, execution_key, ordering_program_wasm, provider};
use arena0_transport::Transport;
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
    TerminalObserved,
    FramesDelivered,
    ReceiptVerified,
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
        let loaded = WasmtimeEngine::new().unwrap().load(&program).unwrap();
        let params = JsonBytes::try_new(br#"{}"#.to_vec()).unwrap();
        let initialized = loaded
            .initialize(InitializeCall::new(params.clone()))
            .unwrap();
        let exec_id = ExecId([0x73; 32]);
        let negotiation_id = NegotiationId([0x74; 32]);
        let activation = activation_for(
            &keys,
            negotiation_id,
            program.hash(),
            params.as_bytes().to_vec(),
            StateHash::of_shared(&initialized.shared),
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
        let mut state = writer.load_execution().await.unwrap().unwrap();
        state.activate().unwrap();
        writer
            .persist(arena0_store::TransitionRecord {
                expected: ExecutionVersion::ZERO,
                next: state.clone(),
                change: arena0_store::Change::Activate,
                now_ms: 6,
            })
            .await
            .unwrap();
        let unsigned = AbortOccurrence::unsigned(
            activation.session_hash(),
            peers[0],
            AbortKind::Abort,
            7,
            "Host stopped before the first public call",
            state.step_cursor(),
        )
        .unwrap();
        let signature = keys[0].sign(&unsigned.signing_bytes().unwrap());
        let expected = state.version();
        state
            .stop(unsigned.with_signature(signature).unwrap())
            .unwrap();
        writer
            .persist(arena0_store::TransitionRecord {
                expected,
                next: state.clone(),
                change: arena0_store::Change::Stop,
                now_ms: 7,
            })
            .await
            .unwrap();
        if matches!(cut, CrashAfter::Publication) {
            let artifact = writer.assemble_receipt(&state).await.unwrap();
            let expected = state.version();
            state.publish_receipt(artifact.clone()).unwrap();
            writer
                .persist(arena0_store::TransitionRecord {
                    expected,
                    next: state,
                    change: arena0_store::Change::Publish { artifact },
                    now_ms: 8,
                })
                .await
                .unwrap();
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
        // The recovery actor owns the producer side of the persisted abort
        // frame. Keep the remote endpoint alive with a transport-only reader
        // so this test exercises the real final-frame acknowledgement boundary
        // without starting a second execution actor or fabricating state.
        let remote_transport = Arc::new(transports.remove(1));
        let remote_ack_task = tokio::spawn(acknowledge_exec_streams(remote_transport));
        let transport = Arc::new(transports.remove(0));
        let identity = Arc::new(provider(7));
        let host = Host::start(Arc::clone(&identity), transport, store.handle().clone());
        let writer = host.claim_execution(exec_id).unwrap();
        let context = ExecContext::new(
            exec_id,
            loaded,
            params,
            activation,
            execution_key(&identity),
        );
        let mut spawned = host.spawn(context, writer).unwrap();
        progress.push((Milestone::RecoveryStarted, started.elapsed()));
        let state = store
            .handle()
            .load_execution(exec_id)
            .await
            .unwrap()
            .expect("durable execution after recovery");
        assert_eq!(state.status().lifecycle(), ExecLifecycle::Aborted);
        progress.push((Milestone::TerminalObserved, started.elapsed()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let receipts = loop {
            let receipts = store.handle().list_receipts(10).await.unwrap();
            let candidates = store
                .handle()
                .list_recovery_candidates(RecoveryCursor::start(), 10)
                .await
                .unwrap();
            if receipts.len() == 1 && candidates.is_empty() {
                break receipts;
            }
            if let Ok(SessionMessage::Failed { reason }) = spawned.message_rx.try_recv() {
                panic!("recovery failed at {cut:?}: {reason}");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "receipt recovery made no progress at {cut:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        progress.push((Milestone::FramesDelivered, started.elapsed()));
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].provenance,
            arena0_store::ReceiptProvenance::Produced
        );
        assert!(matches!(
            receipts[0].receipt,
            arena0_protocol::ReceiptArtifact::StopReport(_)
        ));
        let verified = arena0_verify::verify_light(&receipts[0].receipt.encode().unwrap()).unwrap();
        assert!(matches!(
            verified.terminal,
            arena0_verify::LightVerifiedTerminal::Stopped { .. }
        ));
        progress.push((Milestone::ReceiptVerified, started.elapsed()));
        spawned.shutdown().await;
        remote_ack_task.abort();
        let _ = remote_ack_task.await;
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

/// A transport-only remote seat that acknowledges the producer's stream
/// responsibility. It deliberately does not apply the frame: receipt
/// recovery owns the producer's durable terminal artifact, while this helper
/// only prevents an absent remote actor from masking that delivery boundary.
async fn acknowledge_exec_streams(transport: Arc<LocalTransport>) {
    let Ok(accepted) = transport.accept_exec().await else {
        return;
    };
    let (_, receiver) = accepted.into_parts();
    while let Ok(delivery) = receiver.recv_exec().await {
        let _ = delivery.acknowledge();
    }
}

#[tokio::test]
async fn stopped_execution_recovers_across_receipt_publication_boundaries() {
    for cut in [CrashAfter::Stop, CrashAfter::Publication] {
        recover_after(cut).await;
    }
}
