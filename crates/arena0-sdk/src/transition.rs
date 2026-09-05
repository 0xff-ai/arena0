//! State-machine transition intent returned by program handlers.

use core::convert::Infallible;

/// Abort reason emitted by a program transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortReason {
    reason: String,
}

impl AbortReason {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.reason
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.reason
    }
}

impl From<String> for AbortReason {
    fn from(reason: String) -> Self {
        Self::new(reason)
    }
}

impl From<&str> for AbortReason {
    fn from(reason: &str) -> Self {
        Self::new(reason)
    }
}

/// Lifecycle intent returned by one program dispatch segment.
///
/// `End` is payload-free: the terminal outcome is not chosen by the handler but
/// derived by the program's pure [`outcome`](crate::Program::outcome) projection
/// over final shared state. When a handler returns `End`, the generated dispatch
/// glue computes `Program::outcome(shared)`, serializes it, and emits
/// [`Effect::SessionEnd`](crate::Effect::SessionEnd) carrying those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition<Phase = Infallible> {
    /// Stay in the current program phase.
    Stay,
    /// Move to another program phase.
    To(Phase),
    /// End the session; the outcome is derived from final shared state.
    End,
    /// Abort the session with a reason.
    Abort(AbortReason),
}

impl<Phase> Transition<Phase> {
    #[must_use]
    pub fn abort(reason: impl Into<AbortReason>) -> Self {
        Self::Abort(reason.into())
    }
}

// Hand-written rather than derived: `Stay` is the default for every `Phase`,
// so we must not pick up the `Phase: Default` bound a derive would impose
// (e.g. `Transition<Infallible>` must still be `Default`).
#[allow(clippy::derivable_impls)]
impl<Phase> Default for Transition<Phase> {
    fn default() -> Self {
        Self::Stay
    }
}
