//! Program wasm artifact resolution for the test suite.

use std::path::PathBuf;

/// Read the built wasm artifact for `stem` (e.g. `rock_paper_scissors`).
///
/// Panics with the artifact path and build command when it cannot be read.
pub fn program_wasm(stem: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../programs/target/wasm32-unknown-unknown/release")
        .join(format!("{stem}.wasm"));
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read required guest {}: {error}; run `just build-programs`",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic(expected = "run `just build-programs`")]
    fn missing_guest_fails_with_build_instructions() {
        super::program_wasm("missing-test-guest");
    }
}
