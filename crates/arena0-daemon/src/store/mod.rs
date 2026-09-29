//! Daemon persistence that remains outside the Host's SQLite store.
//!
//! Identity custody is deliberately file-based and separate from the Host's
//! durable protocol state. Program membership, execution state, and receipts
//! are owned by [`arena0_store::Store`].

pub(crate) mod keystore;

pub use keystore::Keystore;
