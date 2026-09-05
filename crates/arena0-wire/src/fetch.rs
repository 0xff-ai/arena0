//! Raw negotiation convergence-fetch wire values.

use borsh::{BorshDeserialize, BorshSerialize};
use std::io;

use super::exec::SessionHashBytes;
use crate::{WireError, read_bounded_bytes_vec, serialize_bounded_vec};

/// Maximum number of tickets carried by one fetch response.
pub const MAX_FETCH_TICKETS: usize = 64;
/// Maximum encoded bytes in one raw ticket payload.
pub const MAX_FETCH_TICKET_BYTES: usize = 4 * 1024;
/// Maximum encoded body bytes in one fetch response, including its kind,
/// routing key, collection length, and per-ticket lengths.
pub const MAX_FETCH_RESPONSE_BYTES: usize = 32 * 1024;
/// Typed fetch message kind for [`FetchFrame::FetchActivationTickets`].
pub const FETCH_KIND_REQUEST: u8 = 0x00;
/// Typed fetch message kind for [`FetchFrame::ActivationTickets`].
pub const FETCH_KIND_RESPONSE: u8 = 0x01;

/// Raw bounded convergence-fetch protocol.
///
/// A fetch is an idempotent one-request/one-response stream correlated by
/// `session_hash`. The negotiation driver owns the wall-clock deadline and
/// retry policy; dropping either stream handle cancels the attempt. The frame
/// itself therefore carries no mutable cancellation state or unbounded queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchFrame {
    /// Request the exact frozen ticket set for one session hash.
    FetchActivationTickets {
        /// Session routing key.
        session_hash: SessionHashBytes,
    },
    /// Return the exact frozen ticket set, in frozen order.
    ActivationTickets {
        /// Session routing key.
        session_hash: SessionHashBytes,
        /// Canonical Borsh bytes for each ticket.
        tickets: Vec<Vec<u8>>,
    },
}

impl BorshSerialize for FetchFrame {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::FetchActivationTickets { session_hash } => {
                BorshSerialize::serialize(&FETCH_KIND_REQUEST, writer)?;
                BorshSerialize::serialize(session_hash, writer)
            }
            Self::ActivationTickets {
                session_hash,
                tickets,
            } => {
                if tickets.len() > MAX_FETCH_TICKETS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        WireError::CollectionTooLarge {
                            field: "fetch.tickets",
                            size: tickets.len(),
                            max: MAX_FETCH_TICKETS,
                        },
                    ));
                }
                for ticket in tickets {
                    if ticket.len() > MAX_FETCH_TICKET_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            WireError::ValueTooLarge {
                                field: "fetch.ticket",
                                size: ticket.len(),
                                max: MAX_FETCH_TICKET_BYTES,
                            },
                        ));
                    }
                }
                let ticket_bytes = tickets
                    .iter()
                    .try_fold(0usize, |total, ticket| total.checked_add(ticket.len()))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            WireError::ValueTooLarge {
                                field: "fetch.response",
                                size: usize::MAX,
                                max: MAX_FETCH_RESPONSE_BYTES,
                            },
                        )
                    })?;
                let encoded_len = 1usize
                    .checked_add(32)
                    .and_then(|size| size.checked_add(4))
                    .and_then(|size| size.checked_add(tickets.len().checked_mul(4)?))
                    .and_then(|size| size.checked_add(ticket_bytes))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            WireError::ValueTooLarge {
                                field: "fetch.response",
                                size: usize::MAX,
                                max: MAX_FETCH_RESPONSE_BYTES,
                            },
                        )
                    })?;
                if encoded_len > MAX_FETCH_RESPONSE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        WireError::ValueTooLarge {
                            field: "fetch.response",
                            size: encoded_len,
                            max: MAX_FETCH_RESPONSE_BYTES,
                        },
                    ));
                }
                BorshSerialize::serialize(&FETCH_KIND_RESPONSE, writer)?;
                BorshSerialize::serialize(session_hash, writer)?;
                serialize_bounded_vec(writer, tickets, MAX_FETCH_TICKETS, "fetch.tickets")
            }
        }
    }
}

impl BorshDeserialize for FetchFrame {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            FETCH_KIND_REQUEST => Ok(Self::FetchActivationTickets {
                session_hash: SessionHashBytes::deserialize_reader(reader)?,
            }),
            FETCH_KIND_RESPONSE => {
                let session_hash = SessionHashBytes::deserialize_reader(reader)?;
                let tickets = read_bounded_bytes_vec(
                    reader,
                    MAX_FETCH_TICKETS,
                    MAX_FETCH_TICKET_BYTES,
                    "fetch.tickets",
                    "fetch.ticket",
                )?;
                let encoded_len = tickets.iter().try_fold(0usize, |total, ticket| {
                    total.checked_add(ticket.len()).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "fetch response size overflow")
                    })
                })?;
                let response_len = 1usize
                    .checked_add(32)
                    .and_then(|size| size.checked_add(4))
                    .and_then(|size| size.checked_add(tickets.len().checked_mul(4)?))
                    .and_then(|size| size.checked_add(encoded_len))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "fetch response size overflow")
                    })?;
                if response_len > MAX_FETCH_RESPONSE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "fetch response body is {response_len} bytes; maximum is {MAX_FETCH_RESPONSE_BYTES}"
                        ),
                    ));
                }
                Ok(Self::ActivationTickets {
                    session_hash,
                    tickets,
                })
            }
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown fetch frame tag {tag}"),
            )),
        }
    }
}

impl crate::sealed::WireDecode for FetchFrame {}
