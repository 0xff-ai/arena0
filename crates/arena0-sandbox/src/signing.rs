//! Synchronous guest signing capability.
//!
//! A dispatch may install one signer for its duration. The signer owns the
//! execution-bound preimage construction, so the guest receives the exact
//! bytes that were signed rather than a value it must reconstruct.

use std::fmt;
use std::sync::Arc;

use arena0_crypto::SignScheme;

/// Signs one guest payload within a dispatch.
///
/// The host is expected to build a versioned, execution-bound preimage from
/// its own coordinates plus `call_index`; the guest payload is data inside
/// that contract, never the preimage itself.
pub trait GuestSigner: Send + Sync {
    /// Sign one guest payload. Returns (exact signed bytes, signature).
    fn sign(
        &self,
        call_index: u32,
        scheme: SignScheme,
        payload: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), String>;
}

/// Per-dispatch signer and sign-call ordinal.
#[derive(Default)]
pub(crate) struct SignerSlot {
    signer: Option<Arc<dyn GuestSigner>>,
    calls: u32,
}

impl fmt::Debug for SignerSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignerSlot")
            .field("present", &self.signer.is_some())
            .field("calls", &self.calls)
            .finish()
    }
}

impl SignerSlot {
    /// Install the signer for the next dispatch and reset its call ordinal.
    pub(crate) fn install(&mut self, signer: Option<Arc<dyn GuestSigner>>) {
        self.signer = signer;
        self.calls = 0;
    }

    /// Drop the signer so a later call kind cannot reuse it.
    pub(crate) fn clear(&mut self) {
        self.signer = None;
        self.calls = 0;
    }

    /// Borrow the installed signer.
    pub(crate) fn signer(&self) -> Option<Arc<dyn GuestSigner>> {
        self.signer.clone()
    }

    /// Return the ordinal of the next sign call in this dispatch, starting at 0.
    pub(crate) fn next_call(&mut self) -> u32 {
        let index = self.calls;
        // The per-dispatch host-call budget rejects a runaway guest long
        // before the ordinal can wrap; saturation only keeps the count honest.
        self.calls = self.calls.saturating_add(1);
        index
    }
}
