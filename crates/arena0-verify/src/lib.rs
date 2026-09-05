//! Verification of the portable arena0 [`arena0_protocol::ReceiptArtifact`] artifact.
//!
//! [`verify_light`] accepts only the bounded, encoded receipt.  It performs the
//! complete activation-chain, trace, and certificate checks and
//! has no sandbox dependency. With the `replay` feature, full verification also
//! accepts the exact Wasm bytes and replays every public call through fresh
//! sandbox-admitted guest calls.

mod error;
mod light;

pub use error::VerifyError;
pub use light::{LightVerified, LightVerifiedTerminal, verify_light};

#[cfg(feature = "replay")]
mod full;
#[cfg(feature = "replay")]
pub use full::{VerifiedOutcome, VerifiedTerminal, verify_full};
