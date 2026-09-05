//! The shared-state commitment.

use crate::id::id_type;

id_type!(
    /// A blake3 hash of program shared state at a point in time.
    ///
    /// The central shared invariant: every participant must produce identical
    /// `Hash` values after every dispatch step. Conceptually a view-root whose
    /// single-segment (symmetric) case is the flat hash of the whole state
    /// buffer; a zero value means "no claim". Construct with [`Hash::of`].
    pub struct Hash,
    Default
);

impl Hash {
    /// Content address of a shared-state snapshot: blake3 of the bytes.
    #[must_use]
    pub fn of(snapshot: &[u8]) -> Self {
        Self(*blake3::hash(snapshot).as_bytes())
    }
}
