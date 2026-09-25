//! Errors returned by receipt verification.

/// A failure while validating a portable authenticated artifact.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The input exceeded the protocol's encoded receipt bound.
    #[error("authenticated artifact is {actual} bytes; maximum is {max}")]
    ReceiptTooLarge { actual: usize, max: usize },
    /// The encoded receipt was not a bounded, version-5 wire value.
    #[error("invalid authenticated artifact: {0}")]
    ReceiptDecode(String),
    /// The receipt failed a protocol or certificate invariant.
    #[error("invalid receipt: {0}")]
    ReceiptInvalid(String),
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
