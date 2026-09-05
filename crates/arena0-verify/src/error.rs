//! Errors returned by receipt verification.

use arena0_program::ProgramHash;

/// A failure while validating a portable authenticated artifact.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The input exceeded the protocol's encoded receipt bound.
    #[error("authenticated artifact is {actual} bytes; maximum is {max}")]
    ReceiptTooLarge { actual: usize, max: usize },
    /// The encoded receipt was not a bounded, version-1 wire value.
    #[error("invalid authenticated artifact: {0}")]
    ReceiptDecode(String),
    /// The receipt failed a protocol or certificate invariant.
    #[error("invalid receipt: {0}")]
    ReceiptInvalid(String),
    /// The encoded receipt body binds parameters different from the activation.
    #[error("receipt parameters do not match the activation")]
    ParamsMismatch,
    /// The receipt has no public trace entries.
    #[error("receipt trace is empty")]
    EmptyTrace,
    /// The state-hash or commitment chain is discontinuous.
    #[error("hash chain broken at step {step}: {message}")]
    ChainBroken { step: u64, message: String },
    /// A public event is not legal at its recorded position.
    #[error("public entry {step} is invalid: {message}")]
    PublicEntryInvalid { step: u64, message: String },
    /// An agreement failed its signer bitmap or aggregate verification.
    #[error("agreement verification failed at step {step}: {message}")]
    Agreement { step: u64, message: String },
    /// A public entry lacks one or more committed participant signatures.
    #[error("public entry {step} lacks a full participant agreement")]
    MissingParticipantAgreement { step: u64 },
    /// The terminal certificate is absent.
    #[error("receipt has no signed terminal certificate")]
    MissingTerminal,
    /// A terminal certificate field disagrees with the final public entry.
    #[error("signed terminal field `{field}` does not match the final public entry")]
    TerminalMismatch { field: &'static str },
    /// The trace carries terminal evidence before its final entry.
    #[error("terminal evidence at step {step} is not the final entry")]
    TerminalNotLast { step: u64 },
    /// The final public entry is not a single successful session end.
    #[error("final public entry has no single SessionEnd outcome")]
    OutcomeMissing,
    /// The terminal's outcome hash is not the hash of the receipt outcome.
    #[error("receipt outcome does not match the terminal outcome hash")]
    OutcomeHashMismatch,
    /// The supplied program is not the program named by the activation.
    #[error("supplied program hashes to {loaded} but receipt attests {attested}")]
    ProgramMismatch {
        /// Hash of the Wasm supplied for replay.
        loaded: ProgramHash,
        /// Hash committed by the activation.
        attested: ProgramHash,
    },
    /// The supplied Wasm is over the sandbox's admission bound.
    #[error("program is {actual} bytes; maximum is {max}")]
    ProgramTooLarge { actual: usize, max: u64 },
    /// The replay sandbox rejected a fresh call.
    #[error("sandbox error: {0}")]
    Sandbox(String),
    /// A fresh replay call disagreed with the recorded public state/effects.
    #[error("replay equivalence failed at step {step}: {message}")]
    ReplayMismatch { step: u64, message: String },
    /// The replayed outcome differs from the recorded opaque outcome bytes.
    #[error("replayed outcome differs from the recorded outcome")]
    OutcomeMismatch,
    /// The activated execution profile is not the local sandbox profile.
    #[error("runtime fingerprint mismatch: attested {attested}, replaying {replaying}")]
    FingerprintMismatch { attested: String, replaying: String },
}

/// Keep cryptographic implementation details out of the public error text.
#[must_use]
pub(crate) fn sanitize_verify_message(message: String) -> String {
    if message.contains("blst:") {
        "signature verification failed".to_owned()
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_hides_blst_details() {
        assert_eq!(
            sanitize_verify_message("verification failed: blst: BAD".to_owned()),
            "signature verification failed"
        );
        assert_eq!(
            sanitize_verify_message("invalid signature".to_owned()),
            "invalid signature"
        );
    }
}
