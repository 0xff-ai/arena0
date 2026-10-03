//! Real guests transfer Host-owned blobs in both directions; each receiver authors its outcome.

use arena0_crypto::{HashAlgorithm, hash};
use arena0_protocol::{BlobHash, MAX_DIRECT_RANGE_BYTES as LEAF_BYTES, StepEvent};
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
    Complete,
    Failed,
}

enum Input {
    Exact(Vec<u8>),
    Replaced { granted: Vec<u8>, file: Vec<u8> },
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
fn params(sender: usize, input: &[u8], result: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "input_sender": Arena::peer_id(2, sender),
        "input_hash": BlobHash(hash(HashAlgorithm::Blake3, input)),
        "input_length": input.len(),
        "result_hash": BlobHash(hash(HashAlgorithm::Blake3, result)),
        "result_length": result.len(),
    }))
    .unwrap()
}

async fn transfer(sender: usize, input: Option<Input>, result: &[u8]) -> Run {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    // An absent import still has nonempty agreed terms: failure must come
    // from the missing source, not from configuration rejection.
    let input_bytes = match &input {
        Some(Input::Exact(bytes)) => bytes.clone(),
        Some(Input::Replaced { granted, .. }) => granted.clone(),
        None => content(7, 50_000),
    };
    let input_complete = matches!(&input, Some(Input::Exact(_)));
    let receiver = 1 - sender;
    let mut arena = Arena::new();
    arena
        .program(program_wasm("verified_transfer"))
        .participants(2)
        .timeout(Duration::from_secs(120))
        .params(params(sender, &input_bytes, result))
        .blob(receiver, result.to_vec());
    match input {
        Some(Input::Exact(bytes)) => {
            arena.blob(sender, bytes);
        }
        Some(Input::Replaced { granted, file }) => {
            arena.replaced_blob(sender, granted, file);
        }
        None => {}
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
                input: if input_complete {
                    Status::Complete
                } else {
                    Status::Failed
                },
                result: Status::Complete,
            },
            "participant {participant}"
        );
        let mut counts = [0usize; 2];
        for entry in run.trace(participant) {
            if let StepEvent::Messages { messages } = entry.event {
                for step_message in &messages {
                    let decoded: Message =
                        borsh::from_slice(&step_message.data).expect("demo agreed message");
                    let (transfer, message, author, complete) = match decoded {
                        Message::Input(message) => (0, message, receiver, input_complete),
                        Message::Result(message) => (1, message, sender, true),
                    };
                    counts[transfer] += 1;
                    assert_eq!(
                        step_message.from,
                        run.peer_id(author),
                        "transfer {transfer}'s receiver authors its outcome"
                    );
                    assert_eq!(matches!(message, TransferMessage::Complete), complete);
                }
            }
        }
        assert_eq!(
            counts,
            [1, 1],
            "participant {participant}: one agreed message per transfer"
        );
    }
    assert_eq!(
        run.read_blob(
            receiver,
            BlobHash(hash(HashAlgorithm::Blake3, &input_bytes))
        )
        .await
        .as_deref(),
        input_complete.then_some(input_bytes.as_slice()),
        "receiver holds exactly the completed input"
    );
    assert_eq!(
        run.read_blob(sender, BlobHash(hash(HashAlgorithm::Blake3, result)))
            .await
            .as_deref(),
        Some(result),
        "receiver's result bytes"
    );
    assert_eq!(run.verify_all().expect("all receipts verify").len(), 2);
    run
}

#[tokio::test]
async fn moves_both_files_when_participant_one_sends_the_input() {
    transfer(
        1,
        Some(Input::Exact(content(1, 1_000_000))),
        &content(2, 777_777),
    )
    .await;
}

#[tokio::test]
async fn participant_zero_sender_and_a_single_leaf_result() {
    transfer(0, Some(Input::Exact(content(3, 100_000))), &content(4, 1)).await;
}

#[tokio::test]
async fn leaf_batches_at_the_message_boundary() {
    transfer(
        0,
        Some(Input::Exact(content(5, 63 * LEAF_BYTES as usize))),
        &content(6, 64 * LEAF_BYTES as usize + 1),
    )
    .await;
}

#[tokio::test]
async fn identical_objects_in_both_directions() {
    let bytes = content(6, 200_000);
    transfer(1, Some(Input::Exact(bytes.clone())), &bytes).await;
}

#[tokio::test]
async fn sender_without_the_blob_fails_its_transfer() {
    transfer(0, None, &content(8, 1024)).await;
}

#[tokio::test]
async fn changed_source_file_fails_at_the_leaf_list() {
    let granted = content(9, 100_000);
    let mut file = granted.clone();
    *file.last_mut().unwrap() ^= 1;
    transfer(
        1,
        Some(Input::Replaced { granted, file }),
        &content(10, 1024),
    )
    .await;
}
