//! The per-node private trace section: local handler runs, entropy draws, and
//! callout answers, hash-committed into the public section per message.
//!
//! Private records never roll up into the shared state hash. Each local run
//! that emits a broadcast stamps the message (and its public trace entry) with
//! the blake3 commitment to the producing [`PrivateRecord`], so reveal is
//! optional but binding: a node that later opens its private section is held
//! to exactly the witness it committed to when it sent the message.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::{PrivateEffect, PrivateEvent};

use super::entry::PendingRecord;

/// A blake3 commitment to one [`PrivateRecord`]: the local witness behind a
/// broadcast message. Travels inside the wire crate's `ExecFrame::Message`
/// and is recorded identically by every node on the message's public entry.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
pub struct WitnessCommitment(pub [u8; 32]);

impl WitnessCommitment {
    /// The blake3 commitment over `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

/// One local (non-shared) handler run in a node's private trace section:
/// callout answers, entropy draws, timers, and decision code reacting to
/// applied public entries. Never part of the byte-identical public section;
/// replay verification of the public trace needs none of this.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PrivateRecord {
    /// Node-local sequence number of this run (0-based, gapless per session).
    pub seq: u64,
    /// Number of public entries this node had applied when the run executed
    /// (the run observed public positions `0..after_position`).
    pub after_position: u64,
    /// The local event that triggered the run.
    pub event: PrivateEvent,
    /// The effects the run emitted (broadcasts, callout requests, timers).
    pub effects: Vec<PrivateEffect>,
    /// Host entropy draws consumed during the run, in draw order, so the node
    /// can replay its own private section deterministically.
    pub draws: Vec<Vec<u8>>,
    /// Fuel consumed by the run (node-local; public-entry fuel is co-signed
    /// separately).
    pub fuel_used: u64,
    /// Pending continuation metadata created by this run, if it suspended on a
    /// callout or a host signature.
    pub pending: Option<PendingRecord>,
}

impl BorshSerialize for PrivateRecord {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.effects.len() > crate::execution::MAX_PRIVATE_EFFECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private effect count exceeds bound",
            ));
        }
        if self.draws.len() > crate::execution::MAX_PRIVATE_EFFECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private draw count exceeds bound",
            ));
        }
        BorshSerialize::serialize(&self.seq, writer)?;
        BorshSerialize::serialize(&self.after_position, writer)?;
        BorshSerialize::serialize(&self.event, writer)?;
        let effect_count = u32::try_from(self.effects.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "effect count overflows u32")
        })?;
        BorshSerialize::serialize(&effect_count, writer)?;
        for effect in &self.effects {
            BorshSerialize::serialize(effect, writer)?;
        }
        let draw_count = u32::try_from(self.draws.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "draw count overflows u32"))?;
        BorshSerialize::serialize(&draw_count, writer)?;
        for draw in &self.draws {
            if draw.len() > crate::execution::MAX_EFFECT_PAYLOAD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private draw exceeds bound",
                ));
            }
            BorshSerialize::serialize(draw, writer)?;
        }
        BorshSerialize::serialize(&self.fuel_used, writer)?;
        BorshSerialize::serialize(&self.pending, writer)
    }
}

impl BorshDeserialize for PrivateRecord {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let seq = u64::deserialize_reader(reader)?;
        let after_position = u64::deserialize_reader(reader)?;
        let event = PrivateEvent::deserialize_reader(reader)?;
        let effect_count = u32::deserialize_reader(reader)? as usize;
        if effect_count > crate::execution::MAX_PRIVATE_EFFECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private effect count exceeds bound",
            ));
        }
        let mut effects = Vec::with_capacity(effect_count);
        for _ in 0..effect_count {
            effects.push(PrivateEffect::deserialize_reader(reader)?);
        }
        let draw_count = u32::deserialize_reader(reader)? as usize;
        if draw_count > crate::execution::MAX_PRIVATE_EFFECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private draw count exceeds bound",
            ));
        }
        let mut draws = Vec::with_capacity(draw_count);
        for _ in 0..draw_count {
            let len = u32::deserialize_reader(reader)? as usize;
            if len > crate::execution::MAX_EFFECT_PAYLOAD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "private draw exceeds bound",
                ));
            }
            let mut draw = vec![0; len];
            reader.read_exact(&mut draw)?;
            draws.push(draw);
        }
        Ok(Self {
            seq,
            after_position,
            event,
            effects,
            draws,
            fuel_used: u64::deserialize_reader(reader)?,
            pending: Option::<PendingRecord>::deserialize_reader(reader)?,
        })
    }
}

impl PrivateRecord {
    /// The blake3 commitment to this record: the witness stamped onto every
    /// broadcast the run emitted.
    #[must_use]
    pub fn witness_commitment(&self) -> WitnessCommitment {
        let bytes = borsh::to_vec(self).expect("PrivateRecord is always serializable");
        WitnessCommitment::of(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn witness_commitment_binds_the_record() {
        let record = PrivateRecord {
            seq: 3,
            after_position: 2,
            event: PrivateEvent::TimerFired,
            effects: Vec::new(),
            draws: vec![vec![1, 2, 3]],
            fuel_used: 7,
            pending: None,
        };
        let commitment = record.witness_commitment();
        let mut tampered = record.clone();
        tampered.draws[0][0] ^= 1;
        assert_ne!(commitment, tampered.witness_commitment());

        let encoded = borsh::to_vec(&record).unwrap();
        let decoded: PrivateRecord = borsh::from_slice(&encoded).unwrap();
        assert_eq!(record, decoded);
        assert_eq!(commitment, decoded.witness_commitment());
    }
}
