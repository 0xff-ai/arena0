//! Daemon persistence that remains outside the Host's SQLite store.
//!
//! Identity custody is deliberately file-based and separate from the Host's
//! durable protocol state. Program membership, execution state, and receipts
//! are owned by [`arena0_store::Store`].

/// Reject control characters, including terminal control sequences, in a
/// human-entered Host name or identity label.
pub(crate) fn validate_plain_name(s: &str) -> Result<(), String> {
    if let Some(c) = s.chars().find(|c| c.is_control()) {
        return Err(format!(
            "name must not contain control characters (found U+{:04X})",
            c as u32
        ));
    }
    Ok(())
}

pub(crate) mod keystore;

pub use keystore::{Keystore, KeystoreError};
