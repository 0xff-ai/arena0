//! Verification of the portable arena0 [`arena0_protocol::ReceiptArtifact`] artifact.
//!
//! [`verify_light`] accepts only the bounded, encoded receipt. It performs the
//! complete activation-chain, trace, and certificate checks without loading a
//! program or sandbox.

mod error;
mod light;

pub use error::VerifyError;
pub use light::{LightVerified, LightVerifiedTerminal, verify_light};
