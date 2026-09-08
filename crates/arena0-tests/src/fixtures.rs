//! Test-local construction helpers shared across the suite: ticket/offer
//! builders and peer/program id helpers.

use std::sync::Arc;

use arena0_crypto::{BlsSignature, ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
use arena0_node::{ExecContext, Host, SpawnedExec};
use arena0_program::{
    Capability, JsonSchemaDocument, ProgramDefinition, ProgramHash, ProgramMetadata, ProgramSchema,
    StateSchema,
};
use arena0_protocol::{
    Activation, ActivationData, ExecId, ExecutionAdmission, MAX_TICKET_LIFETIME_MS, NegotiationId,
    Offer, OfferData, OfferHash, PeerId, PeerIdSource, PreparedActivation, StateHash, Ticket,
    TicketAction, TicketData, TicketHash,
};
use arena0_sandbox::{InitializeCall, Program, WasmtimeEngine};
use arena0_store::{Store, StoreConfig, StoreHandle};
use arena0_transport::local::{LocalNetwork, LocalTransport};
use arena0_transport::{RecvHandle, SendHandle, Transport};
use tempfile::TempDir;

/// Deadline for real Host/SQLite/Wasm convergence checks under parallel test load.
pub const LIVE_EXECUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A deterministic node identity.
pub fn provider(seed: u8) -> NodeKeys {
    NodeKeys::from_secret(SecretKey::from_bytes([seed; 32]))
}

/// A deterministic execution key for one test identity.
pub fn execution_key(identity: &NodeKeys) -> ExecutionKey {
    let scope = identity.ed25519_public_key().0;
    ExecutionKey::derive(
        &ExecutionSalt::try_from_bytes(scope).expect("non-zero test salt"),
        &scope,
        &[0xA0; 32],
    )
    .expect("execution key")
}

/// Build a new-model activation for `cryptos` (sorted by peer id): an offer
/// with the given params and initial state, Active tickets with scope-bound
/// key_bindings, and the collective signature over the exact `ActivationData`.
/// The execution tests exercise the execution layer through this sealed
/// activation; the negotiation driver produces the same shape in production.
pub fn activation_for(
    cryptos: &[NodeKeys],
    negotiation_id: NegotiationId,
    program_hash: ProgramHash,
    params: Vec<u8>,
    initial_state: StateHash,
) -> Activation {
    let creator = cryptos[0].peer_id();
    let offer_data = OfferData::new(
        negotiation_id,
        0,
        creator,
        program_hash,
        arena0_program::ExecutionProfile::current().hash(),
        arena0_program::JsonBytes::try_new(params).expect("valid JSON params"),
        u16::try_from(cryptos.len()).expect("participant count fits u16"),
        initial_state,
        u64::MAX,
    )
    .expect("valid offer data");
    let offer_hash = OfferHash::of(&offer_data);
    let mut tickets: Vec<Ticket> = cryptos
        .iter()
        .map(|crypto| {
            let execution = execution_key(crypto);
            let execution_bls = execution.public_key();
            let key_binding = execution.key_binding(&offer_hash.0, &crypto.ed25519_public_key().0);
            let data = TicketData::new(
                negotiation_id,
                0,
                crypto.peer_id(),
                0,
                TicketAction::Active {
                    execution_bls,
                    key_binding,
                    issued_at_unix_ms: 0,
                    valid_for_ms: u32::try_from(MAX_TICKET_LIFETIME_MS).expect("fits u32"),
                },
            )
            .expect("valid ticket data");
            Ticket {
                signature: crypto.sign(&data.signing_bytes()),
                data,
            }
        })
        .collect();
    // Canonical order: creator first, rest ascending peer id.
    tickets.sort_by_key(|ticket| (ticket.data.signer != creator, ticket.data.signer));
    let ticket_hashes: Vec<TicketHash> = tickets.iter().map(|t| TicketHash::of(&t.data)).collect();
    let activation_data =
        ActivationData::new(offer_hash, ticket_hashes.clone()).expect("activation data");
    let msg = activation_data.signing_bytes();
    let sigs: Vec<BlsSignature> = cryptos
        .iter()
        .map(|crypto| execution_key(crypto).sign(&msg))
        .collect();
    let aggregate = BlsSignature::aggregate(&sigs).expect("aggregate");
    let offer = Offer::new(offer_data, ticket_hashes).expect("valid offer");
    let prepared = PreparedActivation::new(offer, tickets).expect("prepared activation");
    Activation::new(prepared, aggregate).expect("valid activation")
}

/// The collective signature over the activation data signed only by the given
/// participant indices (for consensus-gate tests: a partial aggregate fails
/// verification against the full ticket set).
pub fn partial_aggregate(
    cryptos: &[&NodeKeys],
    activation: &Activation,
    indices: &[usize],
) -> BlsSignature {
    let msg = activation.activation_data().signing_bytes();
    let sigs: Vec<BlsSignature> = indices
        .iter()
        .map(|&i| execution_key(cryptos[i]).sign(&msg))
        .collect();
    BlsSignature::aggregate(&sigs).expect("aggregate")
}

pub fn session_bls_seed(seed: &[u8; 32]) -> [u8; 32] {
    let mut preimage = seed.to_vec();
    preimage.extend_from_slice(b"/arena0/session-bls");
    *blake3::hash(&preimage).as_bytes()
}

/// The node identity for a test seed.
pub fn session_crypto(seed: &[u8; 32]) -> NodeKeys {
    NodeKeys::from_secret(SecretKey::from_bytes(*seed))
}

/// Agent JSON for `Params { target_size }`.
pub fn encode_params(target_size: u32) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "target_size": target_size }))
        .expect("params serialize as JSON")
}

/// A single real Host execution plus raw local transports for the remaining
/// seats. Adversarial integration tests use this to inject authenticated
/// frames while keeping program execution and persistence on the production
/// boundaries.
pub struct LiveExecution {
    pub identity: Arc<NodeKeys>,
    pub peer_ids: Vec<PeerId>,
    pub transports: Vec<Arc<LocalTransport>>,
    pub host: Arc<Host>,
    pub spawned: SpawnedExec,
    pub store: Store,
    pub store_handle: StoreHandle,
    pub directory: TempDir,
    pub wasm: Vec<u8>,
    pub activation: Activation,
    pub exec_id: ExecId,
    pub session_hash: arena0_protocol::SessionHash,
    pub initial_state: StateHash,
    remote_ack_tasks: Vec<tokio::task::JoinHandle<()>>,
    participant_streams: Vec<SendHandle>,
}

/// Build an activated, durable execution on the first peer in canonical order.
/// The returned actor is created only through `Host::spawn`; the SQLite writer
/// capability is moved through request, activation, and execution unchanged.
pub async fn spawn_live_execution(
    wasm: Vec<u8>,
    mut cryptos: Vec<NodeKeys>,
    negotiation_id: NegotiationId,
    exec_id: ExecId,
    params: Vec<u8>,
) -> LiveExecution {
    cryptos.sort_by_key(NodeKeys::peer_id);
    let peer_ids = cryptos.iter().map(NodeKeys::peer_id).collect::<Vec<_>>();
    let program = Program::try_from(wasm.clone()).expect("program");
    let loaded = WasmtimeEngine::new()
        .expect("sandbox engine")
        .load(&program)
        .expect("load program");
    let params_json = arena0_program::JsonBytes::try_new(params.clone()).expect("JSON params");
    let initialized = loaded
        .initialize(InitializeCall::new(params_json.clone()))
        .expect("initialize program");
    let initial_state = StateHash::of(initialized.shared.as_bytes());
    let activation = activation_for(
        &cryptos,
        negotiation_id,
        program.hash(),
        params.clone(),
        initial_state,
    );
    let identity = Arc::new(cryptos.remove(0));

    let network = LocalNetwork::new();
    let mut transport_peers = peer_ids.clone();
    let outsider = PeerId([9; 32]);
    if !transport_peers.contains(&outsider) {
        transport_peers.push(outsider);
    }
    let transports = LocalTransport::create_network(&network, transport_peers)
        .expect("attach local transports")
        .into_iter()
        .map(Arc::new)
        .collect::<Vec<_>>();
    // The adversarial tests intentionally run one real actor and inject facts
    // from raw participant transports.  The actor still publishes its
    // durable signatures to every selected peer, so each remaining endpoint
    // needs a real transport reader to acknowledge those frames.  This keeps
    // the producer's outbox on the production path without introducing a
    // second runtime/store implementation into the fixture.
    let remote_ack_tasks = transports[1..peer_ids.len()]
        .iter()
        .map(|transport| tokio::spawn(acknowledge_exec_streams(Arc::clone(transport))))
        .collect::<Vec<_>>();
    let directory = tempfile::tempdir().expect("temporary store directory");
    let store = Store::open(StoreConfig::new(
        directory.path().join("arena0.sqlite"),
        identity.peer_id(),
    ))
    .expect("open sqlite store");
    let store_handle = store.handle().clone();
    store_handle
        .register_program(wasm.clone(), 1)
        .await
        .expect("register program");
    let host = Host::start(
        Arc::clone(&identity),
        Arc::clone(&transports[0]) as Arc<dyn arena0_transport::Transport + Sync>,
        store_handle.clone(),
    );
    let mut execution_store = host.claim_execution(exec_id).expect("claim execution");
    execution_store
        .create_execution_request(
            program.hash(),
            Some(params_json.clone()),
            ExecutionAdmission::explicit(negotiation_id, peer_ids.clone())
                .expect("explicit admission"),
            2,
        )
        .await
        .expect("create execution request");
    execution_store
        .prepare_activation(activation.prepared().clone(), 3)
        .await
        .expect("prepare activation");
    execution_store
        .commit_activation(activation.clone(), 4)
        .await
        .expect("commit activation");
    let loaded = WasmtimeEngine::new()
        .expect("sandbox engine")
        .load(&program)
        .expect("load program for actor");
    let context = ExecContext::new(
        exec_id,
        loaded,
        params_json,
        activation.clone(),
        execution_key(identity.as_ref()),
    );
    let spawned = host.spawn(context, execution_store).expect("spawn actor");
    // Keep one authenticated stream open per remote participant. The Host
    // treats a participant stream closure as terminal evidence, so tests that
    // inject several frames must reuse these streams instead of dropping a
    // one-frame handle after every send.
    let mut participant_streams = Vec::with_capacity(peer_ids.len().saturating_sub(1));
    for transport in transports.iter().skip(1) {
        participant_streams.push(
            transport
                .open_exec(&peer_ids[0], activation.session_hash())
                .await
                .expect("open participant execution stream"),
        );
    }

    LiveExecution {
        identity,
        peer_ids,
        transports,
        host,
        spawned,
        store,
        store_handle,
        directory,
        wasm,
        session_hash: activation.session_hash(),
        activation,
        exec_id,
        initial_state,
        remote_ack_tasks,
        participant_streams,
    }
}

impl LiveExecution {
    /// Clone the persistent authenticated stream for participant `index`.
    /// Index zero is the local producer and therefore has no inbound stream.
    pub fn participant_stream(&self, index: usize) -> SendHandle {
        self.participant_streams
            .get(index.saturating_sub(1))
            .cloned()
            .expect("remote participant stream")
    }
}

/// Complete the initial shared SessionStarted proposal with all remote
/// signatures. The actor signs its own proposal during startup; this helper
/// supplies the remaining activation participants through transport.
pub async fn establish_live_session(execution: &LiveExecution, cryptos: &[NodeKeys]) {
    let deadline = tokio::time::Instant::now() + LIVE_EXECUTION_TIMEOUT;
    complete_pending_shared(execution, cryptos).await;
    loop {
        let trace = execution
            .store_handle
            .read_trace(execution.exec_id, 0, u64::MAX)
            .await
            .expect("read session trace");
        if !trace.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for session trace"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Supply every remote signature for the actor's current shared proposal.
/// The caller chooses when to release this quorum, which lets receive-side
/// tests observe accepted-but-not-yet-applicable inbox rows first.
pub async fn complete_pending_shared(execution: &LiveExecution, cryptos: &[NodeKeys]) {
    let deadline = tokio::time::Instant::now() + LIVE_EXECUTION_TIMEOUT;
    let commitment = loop {
        if let Some(state) = execution
            .store_handle
            .load_execution(execution.exec_id)
            .await
            .expect("load execution")
            && let Some(proposal) = state.pending_shared()
        {
            break proposal.commitment().clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for session proposal"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    async fn send_signature(
        execution: &LiveExecution,
        index: usize,
        commitment: arena0_protocol::StepCommitment,
        crypto: &NodeKeys,
    ) -> Result<(), arena0_transport::TransportError> {
        let send = execution.participant_stream(index);
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            send.send_exec(&arena0_protocol::ExecFrame::StepSignature {
                signature: execution_key(crypto).sign(&commitment.signing_bytes()),
                commitment,
            }),
        )
        .await
        .map_err(|_| arena0_transport::TransportError::ConnectionClosed)?
    }

    let signature_for = |index: usize| {
        let peer = execution.peer_ids[index];
        let crypto = cryptos
            .iter()
            .find(|candidate| candidate.peer_id() == peer)
            .expect("fixture peer key");
        (index, crypto)
    };
    let mut pending = (1..execution.peer_ids.len()).map(signature_for);
    if let Some((first_index, first_crypto)) = pending.next() {
        let first = send_signature(execution, first_index, commitment.clone(), first_crypto);
        if let Some((second_index, second_crypto)) = pending.next() {
            // The producer's outbox drives every selected peer. Keep the
            // inbound quorum drives live at the same time so one completed
            // transport receipt cannot starve the other participant's frame.
            let second = send_signature(execution, second_index, commitment.clone(), second_crypto);
            let (first_result, second_result) = tokio::join!(first, second);
            first_result.expect("send first session signature");
            second_result.expect("send second session signature");
        } else {
            first.await.expect("send session signature");
        }
        for (index, crypto) in pending {
            send_signature(execution, index, commitment.clone(), crypto)
                .await
                .expect("send session signature");
        }
    }
}

/// Consume producer-originated execution streams for raw participant seats.
/// Each decoded delivery is acknowledged only through the transport receipt;
/// no protocol or guest state is fabricated here.  The parent task owns all
/// reader tasks so dropping [`LiveExecution`] cannot leak detached readers.
async fn acknowledge_exec_streams(transport: Arc<LocalTransport>) {
    let mut readers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = transport.accept_exec() => {
                let Ok(accepted) = accepted else { break };
                let (_, recv) = accepted.into_parts();
                readers.spawn(acknowledge_exec_stream(recv));
            }
            joined = readers.join_next(), if !readers.is_empty() => {
                let _ = joined;
            }
        }
    }
    readers.abort_all();
}

async fn acknowledge_exec_stream(recv: RecvHandle) {
    loop {
        let Ok(delivery) = recv.recv_exec().await else {
            return;
        };
        let _ = delivery.acknowledge();
    }
}

impl Drop for LiveExecution {
    fn drop(&mut self) {
        for task in &self.remote_ack_tasks {
            task.abort();
        }
    }
}

/// Build a minimal real Wasm guest used by transport/ordering tests. It
/// accepts every shared event, selects participant 1 as the next writer, and
/// has no local side effects. The module still crosses the exact metadata,
/// ABI, and fresh-instance sandbox boundary used by production guests.
pub fn ordering_program_wasm(reject_shared: bool) -> Vec<u8> {
    ordering_program(if reject_shared {
        OrderingBehavior::RejectMessage
    } else {
        OrderingBehavior::Accept
    })
}

/// A real ABI guest that unanimously fails on its first shared call.
pub fn shared_stop_program_wasm() -> Vec<u8> {
    ordering_program(OrderingBehavior::Fail)
}

enum OrderingBehavior {
    Accept,
    RejectMessage,
    Fail,
}

fn ordering_program(behavior: OrderingBehavior) -> Vec<u8> {
    let unit = JsonSchemaDocument::unit();
    let definition = ProgramDefinition {
        metadata: ProgramMetadata {
            name: "ordering-fixture".into(),
            version: "1.0.0".into(),
            description: "deterministic transport ordering fixture".into(),
            author: None,
            capabilities: vec![Capability::Messaging],
            display_name: "Ordering fixture".into(),
            participants: arena0_program::ParticipantCount::Exact { count: 3 },
        },
        schema: ProgramSchema {
            state: StateSchema {
                schema: unit.clone(),
                max_bytes: 64,
            },
            callouts: Vec::new(),
            messages: Vec::new(),
            params: unit.clone(),
            queries: Vec::new(),
            outcome: unit,
        },
    };
    let metadata = definition.encode().expect("ordering metadata");
    let init = wat_data(&[0, 0, 0, 0, 0, 0, 0, 0]);
    let shared = wat_data(&[0, 0, 0, 0, 0]);
    let rejected_shared = wat_data(&[1, 0, 0, 0, 0]);
    let local = wat_data(&[0, 0, 0, 0, 0]);
    let writer = wat_data(&[1, 1]);
    let outcome = wat_data(&[0, 0, 0, 0, 4, 0, 0, 0, b'n', b'u', b'l', b'l']);
    let query = wat_data(&[0, 0, 0, 0, 4, 0, 0, 0, b'n', b'u', b'l', b'l']);
    let view = wat_data(&[4, 0, 0, 0, b'n', b'u', b'l', b'l']);
    let metadata_data = wat_data(&metadata);
    let wat = format!(
        r#"
        (module
          (import "arena0" "broadcast" (func $broadcast (param i32 i32)))
          {stop_import}
          (memory (export "memory") 2)
          (global (export "arena0_abi_version") i32 (i32.const 20))
          (data (i32.const 2048) "{init}")
          (data (i32.const 4096) "{shared}")
          (data (i32.const 5120) "{rejected_shared}")
          (data (i32.const 6144) "{local}")
          (data (i32.const 8192) "{writer}")
          (data (i32.const 10240) "{outcome}")
          (data (i32.const 12288) "{query}")
          (data (i32.const 14336) "{view}")
          (data (i32.const 16384) "{metadata_data}")
          (func $pack (param $ptr i32) (param $len i32) (result i64)
            local.get $ptr
            i64.extend_i32_s
            i64.const 32
            i64.shl
            local.get $len
            i64.extend_i32_s
            i64.const 4294967295
            i64.and
            i64.or)
          (func (export "arena0_alloc") (param i32) (result i32) i32.const 1024)
          (func (export "arena0_dealloc") (param i32 i32))
          (func (export "arena0_initialize") (param i32 i32) (result i64)
            i32.const 2048 i32.const 8 call $pack)
          {shared_export}
          (func (export "arena0_local") (param i32 i32) (result i64)
            i32.const 6144 i32.const 5 call $pack)
          (func (export "arena0_writer") (param i32 i32) (result i64)
            i32.const 8192 i32.const 2 call $pack)
          (func (export "arena0_outcome") (param i32 i32) (result i64)
            i32.const 10240 i32.const 12 call $pack)
          (func (export "arena0_query") (param i32 i32) (result i64)
            i32.const 12288 i32.const 12 call $pack)
          (func (export "arena0_view") (param i32 i32) (result i64)
            i32.const 14336 i32.const 8 call $pack)
          (func (export "arena0_metadata") (result i64)
            i32.const 16384 i32.const {metadata_len} call $pack))
        "#,
        init = init,
        shared = shared,
        rejected_shared = rejected_shared,
        stop_import = if matches!(behavior, OrderingBehavior::Fail) {
            r#"(import "arena0" "fail" (func $fail (param i32 i32)))"#
        } else {
            ""
        },
        shared_export = if matches!(behavior, OrderingBehavior::Fail) {
            r#"(func (export "arena0_shared") (param i32 i32) (result i64)
            i32.const 10248 i32.const 4 call $fail
            i32.const 4096 i32.const 5 call $pack)"#
        } else if matches!(behavior, OrderingBehavior::RejectMessage) {
            r#"(func (export "arena0_shared") (param $input_ptr i32) (param i32) (result i64)
            (local $shared_len i32)
            local.get $input_ptr
            i32.load
            local.set $shared_len
            local.get $input_ptr
            i32.const 8
            i32.add
            local.get $shared_len
            i32.add
            i32.load8_u
            if (result i64)
              i32.const 5120 i32.const 5 call $pack
            else
              i32.const 4096 i32.const 5 call $pack
            end)"#
        } else {
            r#"(func (export "arena0_shared") (param i32 i32) (result i64)
            i32.const 4096 i32.const 5 call $pack)"#
        },
        local = local,
        writer = writer,
        outcome = outcome,
        query = query,
        view = view,
        metadata_data = metadata_data,
        metadata_len = metadata.len(),
    );
    append_metadata(&wat::parse_str(wat).expect("ordering Wasm"), &metadata)
}

fn wat_data(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn append_metadata(binary: &[u8], data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + data.len());
    push_leb128(&mut payload, b"arena0.metadata".len() as u64);
    payload.extend_from_slice(b"arena0.metadata");
    payload.extend_from_slice(data);
    let mut out = binary.to_vec();
    out.push(0);
    push_leb128(&mut out, payload.len() as u64);
    out.extend_from_slice(&payload);
    out
}

fn push_leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}
