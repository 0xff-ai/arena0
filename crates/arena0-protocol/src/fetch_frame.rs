//! Validated convergence-fetch frames and their canonical wire encoding.
//!
//! A fetch is an idempotent one-request/one-response stream correlated by
//! `session_hash`. The negotiation driver owns the wall-clock deadline and
//! retry policy; dropping either stream handle cancels the attempt. The frame
//! itself therefore carries no mutable cancellation state or unbounded queue.
//!
//! The first Borsh byte is the frame kind; a response carries each ticket as
//! length-prefixed canonical Borsh bytes. [`arena0_wire::Codec`] adds the
//! versioned length-prefixed envelope.

use arena0_wire::{
    MAX_FETCH_RESPONSE_BYTES, MAX_FETCH_TICKET_BYTES, MAX_FETCH_TICKETS, WireDecode, WireError,
    check_collection_len, read_bounded_bytes, read_collection_len, serialize_bounded_bytes,
    write_collection_len,
};
use borsh::{BorshDeserialize, BorshSerialize};
use std::io;

use crate::{ActivationTickets, FetchActivationTickets, SessionHash, Ticket};

/// Frame kind of [`FetchFrame::FetchActivationTickets`].
const FETCH_KIND_REQUEST: u8 = 0x00;
/// Frame kind of [`FetchFrame::ActivationTickets`].
const FETCH_KIND_RESPONSE: u8 = 0x01;

/// A validated convergence-fetch frame used by protocol machinery.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchFrame {
    /// Request the exact frozen ticket set for one session hash.
    FetchActivationTickets(FetchActivationTickets),
    /// Return the exact frozen ticket set, in frozen order.
    ActivationTickets(ActivationTickets),
}

impl BorshSerialize for FetchFrame {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::FetchActivationTickets(request) => {
                BorshSerialize::serialize(&FETCH_KIND_REQUEST, writer)?;
                BorshSerialize::serialize(&request.session_hash, writer)
            }
            Self::ActivationTickets(response) => {
                check_collection_len(response.tickets.len(), MAX_FETCH_TICKETS, "fetch.tickets")?;
                response
                    .validate()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                // `validate` bounds the frame body size.
                let tickets = response
                    .tickets
                    .iter()
                    .map(borsh::to_vec)
                    .collect::<io::Result<Vec<_>>>()?;
                BorshSerialize::serialize(&FETCH_KIND_RESPONSE, writer)?;
                BorshSerialize::serialize(&response.session_hash, writer)?;
                write_collection_len(writer, tickets.len(), MAX_FETCH_TICKETS, "fetch.tickets")?;
                for ticket in &tickets {
                    serialize_bounded_bytes(
                        writer,
                        ticket,
                        MAX_FETCH_TICKET_BYTES,
                        "fetch.ticket",
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl BorshDeserialize for FetchFrame {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            FETCH_KIND_REQUEST => Ok(Self::FetchActivationTickets(FetchActivationTickets {
                session_hash: SessionHash::deserialize_reader(reader)?,
            })),
            FETCH_KIND_RESPONSE => {
                let session_hash = SessionHash::deserialize_reader(reader)?;
                let count = read_collection_len(reader, MAX_FETCH_TICKETS, "fetch.tickets")?;
                let mut encoded = Vec::with_capacity(count);
                for _ in 0..count {
                    encoded.push(read_bounded_bytes(
                        reader,
                        MAX_FETCH_TICKET_BYTES,
                        "fetch.ticket",
                    )?);
                }
                check_response_len(&encoded, io::ErrorKind::InvalidData)?;
                let invalid = |error| io::Error::new(io::ErrorKind::InvalidData, error);
                let tickets = encoded
                    .iter()
                    .map(|ticket| Ticket::decode(ticket).map_err(invalid))
                    .collect::<io::Result<Vec<_>>>()?;
                let response = ActivationTickets {
                    session_hash,
                    tickets,
                };
                response.validate().map_err(invalid)?;
                Ok(Self::ActivationTickets(response))
            }
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown fetch frame tag {tag}"),
            )),
        }
    }
}

// Every variable-length field is bounded before allocation.
impl WireDecode for FetchFrame {}

/// The encoded body size of a response frame whose tickets encode to
/// `ticket_lens` bytes: kind, routing key, collection length, and each
/// length-prefixed ticket. This is the one size [`MAX_FETCH_RESPONSE_BYTES`]
/// bounds; saturates on overflow.
pub(crate) fn response_body_len(ticket_lens: impl IntoIterator<Item = usize>) -> usize {
    ticket_lens
        .into_iter()
        .try_fold(1 + 32 + 4, |total: usize, len| {
            total.checked_add(4)?.checked_add(len)
        })
        .unwrap_or(usize::MAX)
}

/// Reject encoded tickets whose response body exceeds
/// [`MAX_FETCH_RESPONSE_BYTES`], before any ticket is decoded.
fn check_response_len(tickets: &[Vec<u8>], kind: io::ErrorKind) -> io::Result<()> {
    let size = response_body_len(tickets.iter().map(Vec::len));
    if size > MAX_FETCH_RESPONSE_BYTES {
        return Err(io::Error::new(
            kind,
            WireError::ValueTooLarge {
                field: "fetch.response",
                size,
                max: MAX_FETCH_RESPONSE_BYTES,
            },
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use arena0_crypto::{BlsPublicKey, BlsSignature, Ed25519Signature};
    use arena0_wire::{Codec, StreamProtocol};

    use crate::{NegotiationId, PeerId, TicketAction, TicketData};

    /// The stock-Borsh layout the hand-written frame codec must reproduce.
    #[derive(BorshSerialize)]
    enum DerivedFetchFrame {
        FetchActivationTickets {
            session_hash: SessionHash,
        },
        ActivationTickets {
            session_hash: SessionHash,
            tickets: Vec<Vec<u8>>,
        },
    }

    fn ticket(seed: u8) -> Ticket {
        Ticket {
            data: TicketData::new(
                NegotiationId([0x11; 32]),
                u64::from(seed),
                PeerId([seed; 32]),
                0,
                TicketAction::Active {
                    execution_bls: BlsPublicKey([seed; 96]),
                    key_binding: BlsSignature([seed; 48]),
                    issued_at_unix_ms: 1,
                    valid_for_ms: 60_000,
                },
            )
            .expect("valid ticket data"),
            signature: Ed25519Signature([seed; 64]),
        }
    }

    fn codec() -> Codec {
        Codec::new(StreamProtocol::Fetch.max_frame_body())
    }

    fn raw_frame(body: &[u8]) -> Vec<u8> {
        let body_len = (arena0_wire::FRAME_VERSION_SIZE + body.len()) as u32;
        [
            body_len.to_le_bytes().to_vec(),
            arena0_wire::FRAME_VERSION.to_le_bytes().to_vec(),
            body.to_vec(),
        ]
        .concat()
    }

    #[test]
    fn fetch_frames_preserve_borsh_layout_and_round_trip() {
        let request = SessionHash([13; 32]);
        let response = SessionHash([14; 32]);
        for count in [0, 1, 3, MAX_FETCH_TICKETS] {
            let tickets: Vec<Ticket> = (0..count).map(|seed| ticket(seed as u8)).collect();
            let frame = FetchFrame::ActivationTickets(ActivationTickets {
                session_hash: response,
                tickets: tickets.clone(),
            });
            let derived = DerivedFetchFrame::ActivationTickets {
                session_hash: response,
                tickets: tickets.iter().map(|t| borsh::to_vec(t).unwrap()).collect(),
            };
            assert_eq!(
                borsh::to_vec(&frame).unwrap(),
                borsh::to_vec(&derived).unwrap()
            );
            // `ActivationTickets::validate` bounds exactly this body size.
            assert_eq!(
                response_body_len(tickets.iter().map(|t| borsh::object_length(t).unwrap())),
                borsh::to_vec(&frame).unwrap().len()
            );
            let encoded = codec().encode(&frame).unwrap();
            assert_eq!(codec().decode::<FetchFrame>(&encoded).unwrap(), frame);
        }
        let frame = FetchFrame::FetchActivationTickets(FetchActivationTickets {
            session_hash: request,
        });
        assert_eq!(
            borsh::to_vec(&frame).unwrap(),
            borsh::to_vec(&DerivedFetchFrame::FetchActivationTickets {
                session_hash: request
            })
            .unwrap()
        );
        let encoded = codec().encode(&frame).unwrap();
        assert_eq!(codec().decode::<FetchFrame>(&encoded).unwrap(), frame);
    }

    #[test]
    fn fetch_bounds_are_typed_and_checked_before_allocation() {
        let too_many = FetchFrame::ActivationTickets(ActivationTickets {
            session_hash: SessionHash([1; 32]),
            tickets: (0..=MAX_FETCH_TICKETS)
                .map(|seed| ticket(seed as u8))
                .collect(),
        });
        assert!(matches!(
            codec().encode(&too_many),
            Err(WireError::CollectionTooLarge {
                field: "fetch.tickets",
                ..
            })
        ));

        let mut body = vec![FETCH_KIND_RESPONSE];
        body.extend_from_slice(&[0; 32]);
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            codec().decode::<FetchFrame>(&raw_frame(&body)),
            Err(WireError::ValueTooLarge {
                field: "fetch.ticket",
                ..
            })
        ));

        assert!(matches!(
            codec().decode::<FetchFrame>(&raw_frame(&[0xff])),
            Err(WireError::Decode(_))
        ));
    }
}
