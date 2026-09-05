//! Validated convergence-fetch domain values and their wire conversion.

use arena0_wire::{
    FetchFrame as WireFetchFrame, MAX_FETCH_RESPONSE_BYTES, MAX_FETCH_TICKET_BYTES,
    MAX_FETCH_TICKETS, SessionHashBytes, WireError,
};
use thiserror::Error;

use crate::{ActivationTickets, FetchActivationTickets, NegotiationError, SessionHash, Ticket};

/// A validated convergence-fetch frame used by protocol machinery.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchFrame {
    /// Request the exact frozen ticket set for one session hash.
    FetchActivationTickets(FetchActivationTickets),
    /// Return the exact frozen ticket set, in frozen order.
    ActivationTickets(ActivationTickets),
}

/// Failure converting a convergence-fetch frame across the wire/domain boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchFrameError {
    /// The raw ticket bytes are not a valid bounded protocol ticket.
    #[error("invalid fetch ticket: {0}")]
    Ticket(#[from] NegotiationError),
    /// A wire representation exceeded its explicit bound.
    #[error("invalid fetch frame: {0}")]
    Wire(#[from] WireError),
}

impl TryFrom<WireFetchFrame> for FetchFrame {
    type Error = FetchFrameError;

    fn try_from(frame: WireFetchFrame) -> Result<Self, Self::Error> {
        Ok(match frame {
            WireFetchFrame::FetchActivationTickets { session_hash } => {
                Self::FetchActivationTickets(FetchActivationTickets {
                    session_hash: SessionHash(session_hash.0),
                })
            }
            WireFetchFrame::ActivationTickets {
                session_hash,
                tickets,
            } => {
                if tickets.len() > MAX_FETCH_TICKETS {
                    return Err(WireError::CollectionTooLarge {
                        field: "fetch.tickets",
                        size: tickets.len(),
                        max: MAX_FETCH_TICKETS,
                    }
                    .into());
                }
                let encoded_ticket_bytes = tickets.iter().try_fold(0usize, |total, ticket| {
                    if ticket.len() > MAX_FETCH_TICKET_BYTES {
                        return Err(WireError::ValueTooLarge {
                            field: "fetch.ticket",
                            size: ticket.len(),
                            max: MAX_FETCH_TICKET_BYTES,
                        });
                    }
                    total
                        .checked_add(ticket.len())
                        .ok_or(WireError::ValueTooLarge {
                            field: "fetch.response",
                            size: usize::MAX,
                            max: MAX_FETCH_RESPONSE_BYTES,
                        })
                })?;
                let encoded_response_bytes = 1usize
                    .checked_add(32)
                    .and_then(|size| size.checked_add(4))
                    .and_then(|size| size.checked_add(tickets.len().checked_mul(4)?))
                    .and_then(|size| size.checked_add(encoded_ticket_bytes))
                    .ok_or(WireError::ValueTooLarge {
                        field: "fetch.response",
                        size: usize::MAX,
                        max: MAX_FETCH_RESPONSE_BYTES,
                    })?;
                if encoded_response_bytes > MAX_FETCH_RESPONSE_BYTES {
                    return Err(WireError::ValueTooLarge {
                        field: "fetch.response",
                        size: encoded_response_bytes,
                        max: MAX_FETCH_RESPONSE_BYTES,
                    }
                    .into());
                }
                let mut decoded = Vec::with_capacity(tickets.len());
                for ticket in tickets {
                    decoded.push(Ticket::decode(&ticket)?);
                }
                let response = ActivationTickets {
                    session_hash: SessionHash(session_hash.0),
                    tickets: decoded,
                };
                response.validate()?;
                Self::ActivationTickets(response)
            }
        })
    }
}

impl TryFrom<&FetchFrame> for WireFetchFrame {
    type Error = FetchFrameError;

    fn try_from(frame: &FetchFrame) -> Result<Self, Self::Error> {
        Ok(match frame {
            FetchFrame::FetchActivationTickets(request) => Self::FetchActivationTickets {
                session_hash: SessionHashBytes(request.session_hash.0),
            },
            FetchFrame::ActivationTickets(response) => {
                if response.tickets.len() > MAX_FETCH_TICKETS {
                    return Err(WireError::CollectionTooLarge {
                        field: "fetch.tickets",
                        size: response.tickets.len(),
                        max: MAX_FETCH_TICKETS,
                    }
                    .into());
                }
                response.validate()?;
                let mut tickets = Vec::with_capacity(response.tickets.len());
                let mut total = 0usize;
                for ticket in &response.tickets {
                    let bytes = borsh::to_vec(ticket)
                        .map_err(|error| WireError::Encode(error.to_string()))?;
                    if bytes.len() > MAX_FETCH_TICKET_BYTES {
                        return Err(WireError::ValueTooLarge {
                            field: "fetch.ticket",
                            size: bytes.len(),
                            max: MAX_FETCH_TICKET_BYTES,
                        }
                        .into());
                    }
                    total = total
                        .checked_add(bytes.len())
                        .ok_or(WireError::ValueTooLarge {
                            field: "fetch.response",
                            size: usize::MAX,
                            max: MAX_FETCH_RESPONSE_BYTES,
                        })?;
                    tickets.push(bytes);
                }
                if total > MAX_FETCH_RESPONSE_BYTES {
                    return Err(WireError::ValueTooLarge {
                        field: "fetch.response",
                        size: total,
                        max: MAX_FETCH_RESPONSE_BYTES,
                    }
                    .into());
                }
                Self::ActivationTickets {
                    session_hash: SessionHashBytes(response.session_hash.0),
                    tickets,
                }
            }
        })
    }
}

impl TryFrom<FetchFrame> for WireFetchFrame {
    type Error = FetchFrameError;

    fn try_from(frame: FetchFrame) -> Result<Self, Self::Error> {
        Self::try_from(&frame)
    }
}
