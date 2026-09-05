//! Host-side lifecycle helpers that remain part of the public negotiation API.
//!
//! Execution progress itself is owned by the durable protocol reducer and one
//! private execution actor per execution identity.

pub(crate) mod activation;
pub(crate) mod negotiation;
