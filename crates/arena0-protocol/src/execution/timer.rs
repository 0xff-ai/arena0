use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use super::{MAX_TIMER_PAYLOAD_BYTES, ProtocolError, TIMER_DOMAIN, ensure_payload};
use crate::bounded::read_bytes;
/// Identity of a timer, distinct from an outbox occurrence or protocol frame.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
#[serde(transparent)]
pub struct TimerId([u8; 32]);

impl TimerId {
    /// Derive a timer identity from a caller-supplied stable label.
    #[must_use]
    pub(crate) fn derive(label: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(TIMER_DOMAIN);
        hasher.update(label);
        Self(*hasher.finalize().as_bytes())
    }

    /// Construct an id from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the id bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One active one-shot timer retained in the execution aggregate.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveTimer {
    pub(crate) id: TimerId,
}

/// A timer mutation is the sole owner of timer persistence.  There is no
/// second `RegisterTimer` durable effect that could disagree with this list.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum TimerMutation {
    /// Arm a one-shot timer with a unique occurrence-derived identity.
    Arm {
        /// Timer identity.
        timer_id: TimerId,
        /// Absolute deadline supplied by the adapter input.
        deadline_ms: u64,
        /// Opaque timer payload.
        payload: Vec<u8>,
    },
    /// Consume or cancel exactly one currently active timer identity.
    Cancel {
        /// Timer identity.
        timer_id: TimerId,
    },
}

impl BorshSerialize for TimerMutation {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Self::Arm {
                timer_id,
                deadline_ms,
                payload,
            } => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(timer_id, writer)?;
                BorshSerialize::serialize(deadline_ms, writer)?;
                BorshSerialize::serialize(payload, writer)
            }
            Self::Cancel { timer_id } => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(timer_id, writer)
            }
        }
    }
}

impl BorshDeserialize for TimerMutation {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Arm {
                timer_id: TimerId::deserialize_reader(reader)?,
                deadline_ms: u64::deserialize_reader(reader)?,
                payload: read_bytes(reader, MAX_TIMER_PAYLOAD_BYTES, "timer payload")?,
            }),
            1 => Ok(Self::Cancel {
                timer_id: TimerId::deserialize_reader(reader)?,
            }),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown timer mutation tag {tag}"),
            )),
        }
    }
}

impl TimerMutation {
    /// Construct an arm mutation after checking the payload bound.
    pub fn arm(
        timer_id: TimerId,
        deadline_ms: u64,
        payload: impl Into<Vec<u8>>,
    ) -> Result<Self, ProtocolError> {
        let payload = payload.into();
        ensure_payload("timer payload", payload.len(), MAX_TIMER_PAYLOAD_BYTES)?;
        Ok(Self::Arm {
            timer_id,
            deadline_ms,
            payload,
        })
    }

    /// Construct a cancellation mutation.
    #[must_use]
    pub const fn cancel(timer_id: TimerId) -> Self {
        Self::Cancel { timer_id }
    }

    /// Return the timer identity.
    #[must_use]
    pub const fn timer_id(&self) -> TimerId {
        match self {
            Self::Arm { timer_id, .. } | Self::Cancel { timer_id, .. } => *timer_id,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        if let Self::Arm { payload, .. } = self {
            ensure_payload("timer payload", payload.len(), MAX_TIMER_PAYLOAD_BYTES)?;
        }
        Ok(())
    }
}
