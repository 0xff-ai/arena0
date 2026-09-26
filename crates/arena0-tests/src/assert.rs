//! Trace/agreement assertion helpers: chain recomputation, per-step signing,
//! and store/event polling.

use std::time::Duration;

use arena0_crypto::NodeKeys;
use arena0_protocol::{ExecId, SessionHash, StepCommitment, StepSig, TraceEntry};
use arena0_store::StoreHandle;

/// Recompute each entry's chain-linked commitment left to right.
pub fn commitments(sid: SessionHash, trace: &[TraceEntry]) -> Vec<StepCommitment> {
    let mut link = arena0_protocol::CHAIN_START;
    let mut out = Vec::with_capacity(trace.len());
    for entry in trace {
        let c = StepCommitment::for_entry(sid, entry, link);
        link = c.link_hash();
        out.push(c);
    }
    out
}

/// Sign `commitment` at `step` with `crypto`'s BLS key.
pub fn sign_step(crypto: &NodeKeys, step: u64, commitment: &StepCommitment) -> StepSig {
    StepSig {
        step,
        sig: crate::fixtures::execution_key(crypto).sign(&commitment.signing_bytes()),
    }
}

/// Poll a participant's store until an entry at `step` exists, then return the
/// full trace so far.
pub async fn wait_for_entry(
    store: &StoreHandle,
    execution_id: ExecId,
    step: u64,
) -> Vec<TraceEntry> {
    let deadline = tokio::time::Instant::now() + crate::fixtures::LIVE_EXECUTION_TIMEOUT;
    loop {
        let trace = store
            .read_trace(execution_id, 0, u64::MAX)
            .await
            .expect("read trace");
        if trace.iter().any(|e| e.step == step) {
            return trace;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for entry {step}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
