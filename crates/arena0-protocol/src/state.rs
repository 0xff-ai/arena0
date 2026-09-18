//! The shared-state commitment.

use arena0_program::{SharedStateBytes, canonical_state_image};

use crate::id::id_type;

id_type!(
    /// A blake3 hash of program shared state at a point in time.
    ///
    /// The consensus invariant: participants must produce identical `Hash`
    /// values for each agreed step and [`StepCommitment`](crate::StepCommitment).
    /// A local dispatch may produce a proposed post-state before that step is
    /// agreed. Conceptually a view-root whose single-segment (symmetric) case
    /// is the flat hash of the whole state buffer; a zero value means "no
    /// claim". Construct with [`Hash::of`].
    pub struct Hash,
    Default
);

impl Hash {
    /// Hash arbitrary already-canonical bytes.
    #[must_use]
    pub fn of(snapshot: &[u8]) -> Self {
        Self(*blake3::hash(snapshot).as_bytes())
    }

    /// Hash the complete canonical fixed-size image of shared state.
    ///
    /// Persistent state stores only the bounded payload. Consensus hashes the
    /// same `length || payload || zero_tail` image that the Wasm state memory
    /// contains, so equal payloads always have equal fixed-width commitments.
    #[must_use]
    pub fn of_shared(snapshot: &SharedStateBytes) -> Self {
        let image = canonical_state_image(snapshot.as_bytes())
            .expect("bounded shared state fits the canonical state memory");
        Self::of(&image)
    }
}
