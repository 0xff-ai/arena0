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

/// Error returned by `on_input` handlers and generated awaited callout continuations.
///
/// `?` defaults to `Unrecoverable` (session aborts). Use `.retryable()?`
/// to mark an error as retryable (the runtime re-requests the last input).
///
/// The program macro lowers `ctx.effects().callout(...).await?` into a generated
/// continuation that resumes through the `on_input` dispatch path, even when
/// the source handler's visible signature returns [`ProgramFault`]. Validation
/// after such an await reports `InputFault`; use `.retryable()?` when the
/// answer should be retried.
#[derive(Debug)]
pub enum InputFault {
    /// Unrecoverable error. Session aborts.
    Unrecoverable(anyhow::Error),
    /// Bad agent input. Runtime logs a warning and re-requests the input.
    Retryable(anyhow::Error),
}

impl std::fmt::Display for InputFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unrecoverable(e) | Self::Retryable(e) => write!(f, "{e:#}"),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for InputFault {
    fn from(err: E) -> Self {
        Self::Unrecoverable(err.into())
    }
}

/// Extension trait for marking errors as retryable in `on_input`.
///
/// ```ignore
/// let amount: u32 = text.trim().parse().retryable()?;
/// ```
pub trait Retryable<T> {
    /// Convert this error into a retryable `InputFault`.
    fn retryable(self) -> Result<T, InputFault>;
}

impl<T, E: Into<anyhow::Error>> Retryable<T> for Result<T, E> {
    fn retryable(self) -> Result<T, InputFault> {
        self.map_err(|e| InputFault::Retryable(e.into()))
    }
}

/// Return a retryable error from an `on_input` handler.
///
/// ```ignore
/// if amount > 100 { retryable!("out of range: {amount}"); }
/// ```
#[macro_export]
macro_rules! retryable {
    ($($arg:tt)*) => {{
        use $crate::anyhow::anyhow;
        return Err($crate::InputFault::Retryable(anyhow!($($arg)*)));
    }};
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
        fn input_fault() -> Result<(), InputFault> {
            Err(anyhow!("nope"))?;
            Ok(())
        }
        fn protocol_fault() -> Result<(), ProtocolFault> {
            Err(anyhow!("bad peer"))?;
            Ok(())
        }

        assert!(matches!(program_fault(), Err(ProgramFault(_))));
        assert!(matches!(input_fault(), Err(InputFault::Unrecoverable(_))));
        assert!(matches!(protocol_fault(), Err(ProtocolFault::Malformed(_))));
    }

    #[test]
    fn retryable_macro_produces_retryable() {
        fn try_it() -> Result<(), InputFault> {
            retryable!("bad input");
        }
        assert!(matches!(try_it(), Err(InputFault::Retryable(_))));
    }
}
