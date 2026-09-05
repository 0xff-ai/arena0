//! Sealed-bid Vickrey auction for integer-valued, deterministic settlement.
//!
//! Participant zero is the seller/coordinator and participants one through
//! eight are bidders. Bidders commit and reveal integer bids. The highest bid
//! meeting the optional reserve wins, and the winner's price is the second
//! highest qualifying bid. A tie is resolved from jointly committed entropy.
//! Missing input leaves the auction pending; this guest never consults a clock
//! or performs a payment.

use std::fmt::Write;

use arena0::prelude::*;
use arena0_primitives::commit_reveal::{
    self, CommitReveal, CommitRevealLocal, CommitRevealLocalFieldExt, CommitRevealLocalState,
    CommitRevealSharedFieldExt,
};
use arena0_primitives::joint_randomness;

const MIN_PARTICIPANTS: usize = 3;
const MAX_PARTICIPANTS: usize = 9;
const MAX_ITEM_BYTES: usize = 128;

/// Session parameters for a sealed integer auction.
///
/// Participant zero is the seller/coordinator. Every remaining participant is
/// a bidder. The optional reserve is an integer in the same abstract unit as
/// each bid. The program never performs a payment.
#[arena0::data]
#[derive(Default)]
pub struct Params {
    /// Human-readable label for the auction item.
    pub item: String,
    /// Minimum bid that qualifies for the sale.
    pub reserve: Option<u64>,
}

/// Messages used by the two public commit-reveal rounds.
#[arena0::message]
pub enum Message {
    /// A sealed integer bid, including the seller's fixed no-bid value.
    Bid(commit_reveal::Message<u64>),
    /// Entropy used only when two or more qualifying bidders tie.
    Entropy(commit_reveal::Message<[u8; 32]>),
}

/// Agent-facing callouts.
#[arena0::callouts]
pub enum Callout {
    /// Submit one sealed integer bid for the auction item.
    #[arena0::callout(output = u64)]
    SubmitBid { item: String, reserve: Option<u64> },
}

/// Public program lifecycle. A non-reveal leaves the auction in its current
/// phase. No wall-clock deadline or implicit forfeiture is encoded here.
#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Waiting for the committed auction ensemble")]
    Setup,
    #[phase(description = "Collecting sealed bids")]
    Bidding,
    #[phase(description = "Resolving a tied highest bid")]
    TieBreak,
}

/// Why the auction ended without a sale.
#[arena0::data]
#[derive(Copy)]
pub enum NoSaleReason {
    /// Every revealed bidder bid was below the reserve, or no bidder qualified.
    NoQualifyingBid,
}

/// Shared terminal decision, derived only after all required public messages.
#[arena0::data]
pub enum Settlement {
    /// The winner pays the second-highest qualifying bid. If only one bid
    /// qualifies, the reserve or zero is used as the second price.
    Sold { winner: Participant, price: u64 },
    /// No revealed bidder met the reserve.
    NoSale { reason: NoSaleReason },
}

/// Terminal receipt projected from the final shared state.
///
/// Bids are listed in canonical bidder order, from participant one onward.
/// `Sold` uses the second-highest qualifying bid, with the reserve or zero as
/// the fallback price when exactly one bidder qualifies.
#[arena0::outcome]
pub enum Outcome {
    /// A qualifying bidder won the item.
    Sold {
        item: String,
        reserve: Option<u64>,
        bids: Vec<u64>,
        winner: Participant,
        price: u64,
    },
    /// No bidder met the reserve.
    NoSale {
        item: String,
        reserve: Option<u64>,
        bids: Vec<u64>,
        reason: NoSaleReason,
    },
}

/// Replicated auction state.
///
/// The seller occupies participant zero and contributes a fixed zero bid and
/// fixed zero entropy. This keeps both commit-reveal rounds aligned with the
/// session roster while ensuring only bidders affect the sale and tie seed.
#[arena0::state(max = 32768)]
pub struct Shared {
    #[phase]
    phase: Phase,
    item: String,
    reserve: Option<u64>,
    #[primitive(route = Message::Bid)]
    bids: CommitReveal<u64>,
    #[primitive(route = Message::Entropy)]
    entropy: CommitReveal<[u8; 32]>,
    settlement: Option<Settlement>,
}

/// Participant-local sealed values. These fields never enter the shared hash.
#[arena0::local]
#[derive(Default)]
pub struct Local {
    #[secret]
    bids: CommitRevealLocal<u64>,
    #[secret]
    entropy: CommitRevealLocal<[u8; 32]>,
}

impl CommitRevealLocalState<u64> for Local {
    fn commit_reveal_local(&self) -> &CommitRevealLocal<u64> {
        &self.bids
    }

    fn commit_reveal_local_mut(&mut self) -> &mut CommitRevealLocal<u64> {
        &mut self.bids
    }
}

impl CommitRevealLocalState<[u8; 32]> for Local {
    fn commit_reveal_local(&self) -> &CommitRevealLocal<[u8; 32]> {
        &self.entropy
    }

    fn commit_reveal_local_mut(&mut self) -> &mut CommitRevealLocal<[u8; 32]> {
        &mut self.entropy
    }
}

#[arena0::program(
    name = "vickrey-auction",
    display_name = "Sealed-Bid Vickrey Auction",
    version = "1.0.0",
    description = "Sealed integer bids with deterministic second-price settlement",
    participants = 3..=9,
    capabilities(auto)
)]
pub mod vickrey_auction {
    use super::*;

    type Shared = super::Shared;
    type Local = super::Local;
    type Message = super::Message;
    type Callout = super::Callout;
    type Input = super::Input;
    type Params = super::Params;
    type Outcome = super::Outcome;

    /// Project the terminal receipt from agreed shared state.
    fn outcome(state: &Shared) -> Outcome {
        let bids = state
            .bidder_bids()
            .expect("terminal auction has complete bids");
        match state
            .settlement
            .as_ref()
            .expect("terminal auction has a settlement")
        {
            Settlement::Sold { winner, price } => Outcome::Sold {
                item: state.item.clone(),
                reserve: state.reserve,
                bids,
                winner: *winner,
                price: *price,
            },
            Settlement::NoSale { reason } => Outcome::NoSale {
                item: state.item.clone(),
                reserve: state.reserve,
                bids,
                reason: *reason,
            },
        }
    }

    fn writer(state: &Shared) -> Option<Participant> {
        match state.phase() {
            Phase::Setup => None,
            Phase::Bidding => state.bids.expected_writer(),
            Phase::TieBreak => state.entropy.expected_writer(),
        }
    }

    fn view(ctx: &SharedContext, vp: &Viewport) -> View {
        let state = ctx.shared();
        let mut agents = String::new();
        let mut body = String::new();

        agents.push_str("P0 seller/coordinator\n");
        if state.bids.phase() == commit_reveal::Phase::Complete {
            for (index, bid) in state
                .bidder_bids()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
            {
                let _ = writeln!(agents, "P{} bidder: revealed {}", index + 1, bid);
            }
        } else {
            for index in 1..state.bids.participant_count() {
                let label = bid_status(&state.bids, index);
                let _ = writeln!(agents, "P{index} bidder: {label}");
            }
        }

        if let Some(settlement) = &state.settlement {
            body.push_str("Final revealed bids:\n");
            for (index, bid) in state
                .bidder_bids()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
            {
                let _ = writeln!(body, "P{} bidder: {}", index + 1, bid);
            }
            match settlement {
                Settlement::Sold { winner, price } => {
                    let _ = write!(body, "Sold to P{} for {}", winner.index(), price);
                }
                Settlement::NoSale { .. } => body.push_str("No qualifying bid; no sale"),
            }
        } else {
            match state.phase() {
                Phase::Setup => body.push_str("Waiting for the session ensemble"),
                Phase::Bidding => {
                    let _ = writeln!(
                        body,
                        "Bids: {} of {} commitments collected",
                        count_commits(&state.bids),
                        state.bids.participant_count()
                    );
                    if state.bids.phase() == commit_reveal::Phase::Revealing {
                        body.push_str("Bids are hidden until every reveal arrives.");
                    } else {
                        body.push_str("Waiting for sealed bids.");
                    }
                }
                Phase::TieBreak => {
                    body.push_str("All bids are revealed. Resolving tied highest bids.\n");
                    for (index, bid) in state
                        .bidder_bids()
                        .unwrap_or_default()
                        .into_iter()
                        .enumerate()
                    {
                        let _ = writeln!(body, "P{} bidder: {}", index + 1, bid);
                    }
                    let _ = writeln!(
                        body,
                        "Tie entropy: {} of {} contributions collected",
                        count_commits(&state.entropy),
                        state.entropy.participant_count()
                    );
                }
            }
        }

        let status = if let Some(settlement) = &state.settlement {
            match settlement {
                Settlement::Sold { winner, price } => {
                    format!("complete | sold to P{} for {}", winner.index(), price)
                }
                Settlement::NoSale { .. } => "complete | no sale".to_owned(),
            }
        } else {
            let mut status = format!("{} | reserve: ", phase_label(state.phase()));
            match state.reserve {
                Some(reserve) => status.push_str(&reserve.to_string()),
                None => status.push_str("none"),
            }
            status
        };

        View::new()
            .header(vp.fit_text(format!("Sealed-Bid Vickrey Auction | {}", state.item)))
            .agents(vp.fit_text(agents))
            .state(vp.fit_text(body))
            .status_bar(vp.fit_text(status))
    }

    fn initialize(ctx: &mut SharedContext, params: Params) -> Result<(), ProgramFault> {
        if params.item.trim().is_empty() {
            return Err(anyhow!("item label must not be empty").into());
        }
        if params.item.len() > MAX_ITEM_BYTES {
            return Err(anyhow!("item label must not exceed {MAX_ITEM_BYTES} bytes").into());
        }
        ctx.mutate_shared(|state| {
            state.item = params.item;
            state.reserve = params.reserve;
            state.settlement = None;
        });
        Ok(())
    }

    fn on_session_started(
        ctx: &mut SharedContext,
        ensemble: &Ensemble,
    ) -> Result<Transition<Phase>, ProgramFault> {
        if !(MIN_PARTICIPANTS..=MAX_PARTICIPANTS).contains(&ensemble.len()) {
            return Err(anyhow!(
                "vickrey auction requires {MIN_PARTICIPANTS}..={MAX_PARTICIPANTS} participants"
            )
            .into());
        }
        let count = ensemble.len();
        ctx.mutate_shared(|state| {
            state.bids.set_participant_count(count)?;
            state.entropy.set_participant_count(count)
        })
        .map_err(|error| anyhow!(error))?;
        Ok(Transition::To(Phase::Bidding))
    }

    fn on_react(ctx: &mut Context) -> Result<(), ProgramFault> {
        match ctx.shared().phase() {
            Phase::Setup => {}
            Phase::Bidding => {
                if ctx.shared().bids.expected_writer() != Some(ctx.me()) {
                    return Ok(());
                }
                if let Some(reveal) = ctx.bids().take_reveal() {
                    reveal.broadcast();
                } else if ctx.bids().needs_commit() {
                    if ctx.me().index() == 0 {
                        ctx.bids().commit_with_salt(0, [0; 32])?.broadcast();
                    } else {
                        let request = callouts::SubmitBid {
                            item: ctx.shared().item.clone(),
                            reserve: ctx.shared().reserve,
                        };
                        ctx.effects().callout(request).dispatch();
                    }
                }
            }
            Phase::TieBreak => {
                if ctx.shared().entropy.expected_writer() != Some(ctx.me()) {
                    return Ok(());
                }
                if let Some(reveal) = ctx.entropy().take_reveal() {
                    reveal.broadcast();
                } else if ctx.entropy().needs_commit() {
                    if ctx.me().index() == 0 {
                        ctx.entropy()
                            .commit_with_salt([0; 32], [0; 32])?
                            .broadcast();
                    } else {
                        let nonce = ctx.random_bytes::<32>();
                        ctx.entropy().commit(nonce)?.broadcast();
                    }
                }
            }
        }
        Ok(())
    }

    fn on_message(
        ctx: &mut SharedContext,
        from: Participant,
        message: Message,
    ) -> Result<ApplyDecision<Phase>, ProtocolFault> {
        match message {
            Message::Bid(message) => {
                if ctx.shared().phase() != Phase::Bidding
                    || ctx.shared().bids.expected_writer() != Some(from)
                {
                    return Ok(ApplyDecision::Reject);
                }
                if ctx.bids().handle(from, message).is_err() {
                    return Ok(ApplyDecision::Reject);
                }
                if !ctx.shared().bids.is_complete() {
                    return Ok(ApplyDecision::Accept(Transition::Stay));
                }

                let qualifying = ctx.shared().qualifying_bids().unwrap_or_default();
                match qualifying.len() {
                    0 => {
                        ctx.mutate_shared(|state| {
                            state.settlement = Some(Settlement::NoSale {
                                reason: NoSaleReason::NoQualifyingBid,
                            });
                        });
                        Ok(ApplyDecision::Accept(Transition::End))
                    }
                    1 => {
                        let winner = qualifying[0].0;
                        let price = ctx.shared().second_price();
                        ctx.mutate_shared(|state| {
                            state.settlement = Some(Settlement::Sold { winner, price });
                        });
                        Ok(ApplyDecision::Accept(Transition::End))
                    }
                    _ => {
                        let highest = qualifying
                            .iter()
                            .map(|(_, bid)| *bid)
                            .max()
                            .expect("nonempty qualifying bids");
                        let tied = qualifying.iter().filter(|(_, bid)| *bid == highest).count();
                        if tied < 2 {
                            let winner = qualifying
                                .iter()
                                .find(|(_, bid)| *bid == highest)
                                .expect("highest qualifying bid exists")
                                .0;
                            let price = ctx.shared().second_price();
                            ctx.mutate_shared(|state| {
                                state.settlement = Some(Settlement::Sold { winner, price });
                            });
                            Ok(ApplyDecision::Accept(Transition::End))
                        } else {
                            Ok(ApplyDecision::Accept(Transition::To(Phase::TieBreak)))
                        }
                    }
                }
            }
            Message::Entropy(message) => {
                if ctx.shared().phase() != Phase::TieBreak
                    || ctx.shared().entropy.expected_writer() != Some(from)
                {
                    return Ok(ApplyDecision::Reject);
                }
                if ctx.entropy().handle(from, message).is_err() {
                    return Ok(ApplyDecision::Reject);
                }
                if !ctx.shared().entropy.is_complete() {
                    return Ok(ApplyDecision::Accept(Transition::Stay));
                }
                ctx.mutate_shared(|state| state.settle_tie())
                    .map_err(ProtocolFault::shared_violation)?;
                Ok(ApplyDecision::Accept(Transition::End))
            }
        }
    }

    fn on_input(ctx: &mut Context, input: Input) -> Result<(), InputFault> {
        let Input::SubmitBid(amount) = input;
        if ctx.me().index() == 0 {
            return Err(anyhow!("seller/coordinator cannot submit a bid").into());
        }
        if ctx.shared().phase() != Phase::Bidding {
            return Err(anyhow!("bidding is closed").into());
        }
        ctx.bids().commit(amount)?.broadcast();
        Ok(())
    }

    fn on_query(_ctx: &SharedContext, _: ()) {}

    fn count_commits<T>(protocol: &CommitReveal<T>) -> usize {
        if protocol.phase() == commit_reveal::Phase::Idle {
            protocol
                .expected_writer()
                .map_or(protocol.participant_count(), |participant| {
                    participant.index()
                })
        } else {
            protocol.participant_count()
        }
    }

    fn bid_status(protocol: &CommitReveal<u64>, index: usize) -> &'static str {
        match protocol.phase() {
            commit_reveal::Phase::Complete => "revealed",
            commit_reveal::Phase::Revealing => match protocol.expected_writer() {
                Some(expected) if expected.index() == index => "pending reveal",
                Some(expected) if expected.index() < index => "waiting reveal",
                _ => "revealed",
            },
            commit_reveal::Phase::Idle => match protocol.expected_writer() {
                Some(expected) if expected.index() == index => "pending",
                Some(expected) if expected.index() < index => "waiting",
                _ => "committed",
            },
        }
    }

    fn phase_label(phase: Phase) -> &'static str {
        match phase {
            Phase::Setup => "setup",
            Phase::Bidding => "bidding",
            Phase::TieBreak => "tie-break",
        }
    }
}

impl Shared {
    fn bidder_bids(&self) -> Option<Vec<u64>> {
        self.bids
            .values()
            .map(|values| values.into_iter().skip(1).copied().collect())
    }

    fn qualifying_bids(&self) -> Option<Vec<(Participant, u64)>> {
        let reserve = self.reserve.unwrap_or(0);
        self.bids.values().map(|values| {
            values
                .into_iter()
                .enumerate()
                .skip(1)
                .filter_map(|(index, bid)| {
                    (*bid >= reserve).then_some((
                        Participant::try_from(index).expect("auction participant fits in u8"),
                        *bid,
                    ))
                })
                .collect()
        })
    }

    fn second_price(&self) -> u64 {
        let mut bids: Vec<u64> = self
            .qualifying_bids()
            .expect("second price requires complete bids")
            .into_iter()
            .map(|(_, bid)| bid)
            .collect();
        bids.sort_unstable_by(|left, right| right.cmp(left));
        bids.get(1)
            .copied()
            .unwrap_or_else(|| self.reserve.unwrap_or(0))
    }

    fn settle_tie(&mut self) -> Result<(), arena0::anyhow::Error> {
        let qualifying = self
            .qualifying_bids()
            .ok_or_else(|| anyhow!("tie resolution requires complete bids"))?;
        let highest = qualifying
            .iter()
            .map(|(_, bid)| *bid)
            .max()
            .ok_or_else(|| anyhow!("tie resolution requires a qualifying bid"))?;
        let mut tied: Vec<Participant> = qualifying
            .into_iter()
            .filter_map(|(participant, bid)| (bid == highest).then_some(participant))
            .collect();
        if tied.len() < 2 {
            return Err(anyhow!("tie resolution requires at least two bidders"));
        }
        joint_randomness::shuffle(&self.entropy, &mut tied)
            .ok_or_else(|| anyhow!("joint randomness did not produce a tie order"))?;
        self.settlement = Some(Settlement::Sold {
            winner: tied[0],
            price: self.second_price(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0::ManagedPhase;
    use arena0::types::{ColorDepth, Slot};
    use borsh::BorshSerialize;

    fn complete_protocol<T>(values: &[T]) -> CommitReveal<T>
    where
        T: BorshSerialize + Clone + Default,
    {
        let mut protocol = CommitReveal::default();
        protocol.set_participant_count(values.len()).unwrap();
        let mut locals: Vec<CommitRevealLocal<T>> = values
            .iter()
            .map(|_| CommitRevealLocal::default())
            .collect();
        let commits: Vec<_> = values
            .iter()
            .zip(locals.iter_mut())
            .enumerate()
            .map(|(index, (value, local))| {
                protocol
                    .commit_with_salt(local, value.clone(), [index as u8; 32])
                    .unwrap()
            })
            .collect();
        for (index, commit) in commits.into_iter().enumerate() {
            protocol
                .handle(
                    Participant::try_from(index).expect("test participant fits"),
                    commit,
                )
                .unwrap();
        }
        let reveals: Vec<_> = locals
            .iter_mut()
            .map(|local| protocol.take_reveal(local).unwrap())
            .collect();
        for (index, reveal) in reveals.into_iter().enumerate() {
            protocol
                .handle(
                    Participant::try_from(index).expect("test participant fits"),
                    reveal,
                )
                .unwrap();
        }
        protocol
    }

    fn complete_bids(reserve: Option<u64>, bids: &[u64]) -> Shared {
        let mut state = Shared {
            item: "demo item".into(),
            reserve,
            ..Shared::default()
        };
        let mut values = Vec::with_capacity(bids.len() + 1);
        values.push(0);
        values.extend_from_slice(bids);
        state.bids = complete_protocol(&values);
        state
    }

    #[test]
    fn one_bidder_uses_reserve_as_second_price() {
        let state = complete_bids(Some(50), &[120]);
        assert_eq!(state.second_price(), 50);
    }

    #[test]
    fn no_bidder_qualifies() {
        let state = complete_bids(Some(100), &[40, 90]);
        assert!(state.qualifying_bids().unwrap().is_empty());
    }

    #[test]
    fn tied_winner_is_deterministic_and_price_is_second_highest() {
        let mut first = complete_bids(None, &[120, 180, 180, 90]);
        let mut second = complete_bids(None, &[120, 180, 180, 90]);
        first.entropy = complete_protocol(&[[0; 32], [1; 32], [2; 32], [3; 32], [4; 32]]);
        second.entropy = complete_protocol(&[[0; 32], [1; 32], [2; 32], [3; 32], [4; 32]]);
        first.settle_tie().unwrap();
        second.settle_tie().unwrap();
        assert_eq!(first.settlement, second.settlement);
        assert_eq!(
            first.settlement,
            Some(Settlement::Sold {
                winner: Participant::new(3),
                price: 180,
            })
        );
    }

    #[test]
    fn missing_reveal_keeps_auction_pending() {
        let values = [0, 120, 80];
        let mut bids = CommitReveal::default();
        bids.set_participant_count(values.len()).unwrap();
        let mut locals: Vec<CommitRevealLocal<u64>> = values
            .iter()
            .map(|_| CommitRevealLocal::default())
            .collect();
        let commits: Vec<_> = values
            .iter()
            .zip(locals.iter_mut())
            .enumerate()
            .map(|(index, (value, local))| {
                bids.commit_with_salt(local, *value, [index as u8; 32])
                    .unwrap()
            })
            .collect();
        for (index, commit) in commits.into_iter().enumerate() {
            bids.handle(Participant::try_from(index).unwrap(), commit)
                .unwrap();
        }
        for (index, local) in locals.iter_mut().take(2).enumerate() {
            let reveal = bids.take_reveal(local).unwrap();
            bids.handle(Participant::try_from(index).unwrap(), reveal)
                .unwrap();
        }

        let state = Shared {
            phase: ManagedPhase::__new(Phase::Bidding),
            item: "demo item".into(),
            bids,
            ..Shared::default()
        };
        assert_eq!(state.bids.phase(), commit_reveal::Phase::Revealing);
        assert_eq!(state.bids.expected_writer(), Some(Participant::new(2)));
        assert!(state.settlement.is_none());
    }

    #[test]
    fn view_fills_all_slots_and_mono_has_no_escape_sequences() {
        let mut state = complete_bids(Some(50), &[120, 80]);
        state.settlement = Some(Settlement::Sold {
            winner: Participant::new(1),
            price: 80,
        });
        let ctx = SharedContext::__new(state, None);
        for color in [ColorDepth::Mono, ColorDepth::Ansi16, ColorDepth::TrueColor] {
            let view = <vickrey_auction::VickreyAuction as ProgramView>::view(
                &ctx,
                &Viewport { width: 120, color },
            );
            assert_eq!(view.slots.len(), 4);
            assert!(view.slots.contains_key(&Slot::Header));
            assert!(view.slots.contains_key(&Slot::Agents));
            assert!(view.slots.contains_key(&Slot::State));
            assert!(view.slots.contains_key(&Slot::StatusBar));
            assert!(view.slots[&Slot::State].contains("Final revealed bids"));
            assert!(view.slots[&Slot::State].contains("Sold to P1 for 80"));
            assert_eq!(view.slots[&Slot::StatusBar], "complete | sold to P1 for 80");
            if color == ColorDepth::Mono {
                assert!(view.slots.values().all(|text| !text.contains("\x1b[")));
            }
        }
    }

    #[test]
    fn item_validation_is_bounded() {
        let mut ctx = SharedContext::__new(Shared::default(), None);
        assert!(
            <vickrey_auction::VickreyAuction as Program>::initialize(
                &mut ctx,
                Params {
                    item: " ".into(),
                    reserve: None,
                },
            )
            .is_err()
        );
        assert!(
            <vickrey_auction::VickreyAuction as Program>::initialize(
                &mut ctx,
                Params {
                    item: "x".repeat(MAX_ITEM_BYTES + 1),
                    reserve: None,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn outcome_contains_canonical_bid_order() {
        let mut state = complete_bids(Some(50), &[120, 80]);
        state.settlement = Some(Settlement::Sold {
            winner: Participant::new(1),
            price: 80,
        });
        let outcome = <vickrey_auction::VickreyAuction as Program>::outcome(&state);
        assert!(matches!(
            outcome,
            Outcome::Sold {
                bids,
                winner,
                price: 80,
                ..
            } if bids == vec![120, 80] && winner == Participant::new(1)
        ));
    }
}
