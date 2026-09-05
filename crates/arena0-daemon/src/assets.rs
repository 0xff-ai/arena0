//! Program wasm embedded at build time (see `build.rs`), so a boot seeds the
//! registry from the binary alone, with no source tree or network.

include!(concat!(env!("OUT_DIR"), "/embedded_programs.rs"));
