//! Real guests transfer Host-owned blobs in both directions and certify their progress.

use arena0_crypto::{HashAlgorithm, hash};
use arena0_protocol::{BlobHash, StepEvent};
use arena0_tests::{
    arena::{Arena, Run},
    wasm::program_wasm,
};
use borsh::BorshDeserialize;
use std::time::Duration;

// These mirrors describe only the demo's public Borsh messages and outcome.
// The tests intentionally do not link the native program implementation.
#[derive(Debug, PartialEq, Eq, BorshDeserialize)]
enum Status {
    Complete,
    Failed,
}
#[derive(Debug, PartialEq, Eq, BorshDeserialize)]
struct Outcome {
    input: Status,
    result: Status,
}
#[derive(BorshDeserialize)]
enum Message {
    Input(TransferMessage),
    Result(TransferMessage),
}
#[derive(BorshDeserialize)]
enum TransferMessage {
    Checkpoint(Signed),
    Failed,
}
#[derive(BorshDeserialize)]
struct Signed {
    signed_bytes: Vec<u8>,
    signature: Vec<u8>,
}

/// Deterministic counter-seeded xorshift content, distinct per seed.
fn content(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_add(0x9e3779b97f4a7c15);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// The demo consumes object identities and lengths, never file bytes.
fn params(input: &[u8], result: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "input_hash": BlobHash(hash(HashAlgorithm::Blake3, input)),
        "input_length": input.len(),
        "result_hash": BlobHash(hash(HashAlgorithm::Blake3, result)),
        "result_length": result.len(),
    }))
    .unwrap()
}

async fn transfer(input: &[u8], result: &[u8], seed_input: bool) -> Run {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    let mut arena = Arena::new();
    arena
        .program(program_wasm("verified_transfer"))
        .participants(2)
        .timeout(Duration::from_secs(120))
        .params(params(input, result))
        .blob(1, result.to_vec());
    if seed_input {
        arena.blob(0, input.to_vec());
    }
    let mut run = arena.run().await;
    run.expect_completed_all().await;
    for participant in 0..2 {
        let outcome: Outcome = borsh::from_slice(
            &run.completed_outcome(participant)
                .expect("completed outcome"),
        )
        .unwrap();
        assert_eq!(
            outcome,
            Outcome {
                input: if seed_input {
                    Status::Complete
                } else {
                    Status::Failed
                },
                result: Status::Complete,
            },
            "participant {participant}"
        );
    }
    if seed_input {
        assert_eq!(
            run.read_blob(1, BlobHash(hash(HashAlgorithm::Blake3, input)))
                .await
                .as_deref(),
            Some(input),
            "receiver's input bytes"
        );
    }
    assert_eq!(
        run.read_blob(0, BlobHash(hash(HashAlgorithm::Blake3, result)))
            .await
            .as_deref(),
        Some(result),
        "receiver's result bytes"
    );
    assert_eq!(run.verify_all().expect("all receipts verify").len(), 2);
    run
}

#[tokio::test]
async fn verified_transfer_moves_both_files_and_verifies() {
    let run = transfer(&content(1, 1_000_000), &content(2, 1_000_000), true).await;
    for participant in 0..2 {
        let mut counts = [0usize; 2];
        for entry in run.trace(participant) {
            if let StepEvent::Message { data, .. } = entry.event {
                let message: Message = borsh::from_slice(&data).expect("demo agreed message");
                let (transfer, message) = match message {
                    Message::Input(message) => (0, message),
                    Message::Result(message) => (1, message),
                };
                if let TransferMessage::Checkpoint(signed) = message {
                    assert!(!signed.signed_bytes.is_empty());
                    assert!(!signed.signature.is_empty());
                    counts[transfer] += 1;
                }
            }
        }
        assert!(
            counts[0] >= 2 && counts[1] >= 2,
            "participant {participant}: checkpoints per transfer {counts:?}"
        );
    }
}

#[tokio::test]
async fn verified_transfer_handles_leaf_boundaries_and_a_short_last_chunk() {
    transfer(&content(3, 150_000), &content(4, 50_001), true).await;
}

#[tokio::test]
async fn verified_transfer_moves_an_empty_object() {
    transfer(&[], &content(5, 1024), true).await;
}

#[tokio::test]
async fn verified_transfer_with_identical_objects_in_both_directions() {
    let bytes = content(6, 200_000);
    transfer(&bytes, &bytes, true).await;
}

#[tokio::test]
async fn verified_transfer_fails_when_the_sender_lacks_the_file() {
    transfer(&content(7, 50_000), &content(8, 1024), false).await;
}
