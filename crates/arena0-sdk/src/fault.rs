//! Error types for program handlers.

use arena0_protocol::Participant;

/// Error returned by all program handlers except `on_input`.
///
/// Any error propagated with `?` becomes an abort. The runtime
/// terminates the session when this is returned.
#[derive(Debug)]
pub struct ProgramFault(pub anyhow::Error);

impl std::fmt::Display for ProgramFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl<E: Into<anyhow::Error>> From<E> for ProgramFault {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

/// Error returned by peer-message handlers.
///
/// Peer-message faults are separate from local input faults because the
/// recovery boundary is different: bad local input can be retried, while a
/// malformed or out-of-order peer message is a protocol violation.
#[derive(Debug)]
pub enum ProtocolFault {
    /// The peer sent a message that is not valid in the current state.
    UnexpectedMessage { from: Participant },
    /// The peer message could not be decoded or validated as protocol data.
    Malformed(anyhow::Error),
    /// The peer violated a shared or mirroring rule.
    SharedViolation(anyhow::Error),
}

impl ProtocolFault {
    /// Build an unexpected-message fault for the given sender.
    #[must_use]
    pub fn unexpected_message(from: Participant) -> Self {
        Self::UnexpectedMessage { from }
    }

    /// Build a malformed-message fault.
    pub fn malformed(err: impl Into<anyhow::Error>) -> Self {
        Self::Malformed(err.into())
    }

    /// Build a shared-violation fault.
    pub fn shared_violation(err: impl Into<anyhow::Error>) -> Self {
        Self::SharedViolation(err.into())
    }
}

impl std::fmt::Display for ProtocolFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedMessage { from } => {
                write!(
                    f,
                    "unexpected peer message from participant {}",
                    from.index()
                )
            }
            Self::Malformed(err) => write!(f, "malformed peer message: {err:#}"),
            Self::SharedViolation(err) => write!(f, "shared violation: {err:#}"),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for ProtocolFault {
    fn from(err: E) -> Self {
        Self::Malformed(err.into())
    }
}

impl From<ProgramFault> for ProtocolFault {
    fn from(err: ProgramFault) -> Self {
        Self::Malformed(err.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn error_conversions_preserve_fault_classification() {
        fn program_fault() -> Result<(), ProgramFault> {
            Err(anyhow!("oops"))?;
            Ok(())
        }
        fn protocol_fault() -> Result<(), ProtocolFault> {
            Err(anyhow!("bad peer"))?;
            Ok(())
        }

        assert!(matches!(program_fault(), Err(ProgramFault(_))));
        assert!(matches!(protocol_fault(), Err(ProtocolFault::Malformed(_))));
    }
}
