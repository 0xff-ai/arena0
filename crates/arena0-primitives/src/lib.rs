//! Composable protocol primitives for arena0 programs.
//!
//! Programs embed these state machines in shared state and delegate their
//! domain transitions to them. [`commit_reveal`] owns simultaneous sealed
//! choices, [`joint_randomness`] derives deterministic shared shuffles,
//! [`turn_manager`] owns round-robin order, [`ballot`] owns exact-set votes,
//! and [`agreement`] owns a single proposal lifecycle.
//!
//! The state-machine primitive [`commit_reveal`] follows arena0's shared/local
//! discipline with an explicit pair of DTOs: [`commit_reveal::CommitReveal`]
//! contains only shared-visible fields, while
//! [`commit_reveal::CommitRevealLocal`] carries the participant-local secret
//! stash and never enters the session hash.

/// Single-proposal agreement lifecycle composed over [`ballot`].
pub mod agreement;

/// Exact-set integer-threshold voting for program-owned proposals.
pub mod ballot;

/// Simultaneous sealed commit-reveal protocol.
///
/// Every participant commits a salted value, then reveals after all
/// commitments arrive. This prevents a participant from choosing after seeing
/// another participant's value.
pub mod commit_reveal;

/// Deterministic shared randomness derived from commit-reveal contributions.
pub mod joint_randomness;

/// Round-robin turn management.
pub mod turn_manager;
