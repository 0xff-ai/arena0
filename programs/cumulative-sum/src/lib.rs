//! N-party cumulative sum: every participant draws a random contribution,
//! broadcasts it to the rest of the ensemble, and all accumulate the same total.
//!
//! This is the symmetric multiparty program: the shared state (the final total)
//! converges identically across every node, while the random draws exercise the
//! live entropy path and the broadcast exercises N-party routing.
//! Membership is formed by admission, not in-program: the runtime hands the
//! sealed committed ensemble to `on_session_started` and keeps it available
//! through `ctx.ensemble()`. Every participant BLS-signs the activation commitment that
//! carries the full roster, so membership is agreed cryptographically; the
//! program reads the ensemble for routing and buffer sizing but never re-hashes
//! it into its own shared state. The dispatch glue resolves a message sender to
//! its participant index against that committed ensemble, so the program needs
//! neither the bilateral `ctx` participant helpers nor a host-injected side
//! channel.
//!
//! Contributions live in shared state now: canonical (collect) ordering applies
//! each participant's broadcast at the same public position on every node, so the
//! shared hash moves on every contribution and each is a genuinely co-signed
//! transition. The participant whose slot is next draws its contribution and
//! queues it; the author applies its own message through the same `on_message`
//! handler every receiver runs, so the draw stays deterministic per node
//! (guarded by a local `sent` flag).

use std::fmt::Write;

use arena0::prelude::*;

/// Session parameters: expected membership size.
#[arena0::data]
pub struct Params {
    /// Number of participants expected in the committed session.
    pub target_size: u32,
}

#[arena0::message]
pub enum Message {
    /// A participant's contribution. The sender's slot is taken from the
    /// authenticated `from: Participant` at receipt, never a self-declared field.
    Contribute { value: u64 },
}

#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Collecting contributions")]
    Collecting,
}

/// Derived terminal receipt: the agreed total.
#[arena0::outcome]
pub enum Outcome {
    Sum { total: u64 },
}

#[arena0::state(max = 1024)]
pub struct Shared {
    #[phase]
    phase: Phase,
    total: u64,
    finalized: bool,
    /// Contributions gathered so far, indexed by participant. Shared and hashed:
    /// canonical ordering applies each at the same public position on every node,
    /// so the hash moves in lockstep and every value is a co-signed transition.
    contributions: Vec<Option<u64>>,
}

/// Participant-local decision state.
#[arena0::local]
#[derive(Default)]
pub struct Local {
    /// Whether this node has already drawn and broadcast its contribution.
    sent: bool,
}

impl Shared {
    /// The participant whose contribution the current base is waiting for: the
    /// first empty slot in the contributions buffer. Unique-writer rule: only
    /// that participant's message applies; any other sender is a deterministic
    /// reject (no sibling candidates at one position).
    fn expected_writer(&self) -> Option<usize> {
        self.contributions.iter().position(Option::is_none)
    }

    fn display_total(&self) -> u64 {
        if self.finalized {
            self.total
        } else {
            self.contributions.iter().flatten().sum()
        }
    }
}

#[arena0::program(
    name = "cumulative-sum",
    display_name = "Cumulative Sum Tutorial",
    version = "1.0.0",
    description = "N-party cumulative sum of random contributions",
    participants = 2..=64,
    capabilities(auto)
)]
pub mod cumulative_sum {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;
    type Message = super::Message;
    type Params = super::Params;
    type Outcome = super::Outcome;

    /// Pure projection from final shared state.
    fn outcome(state: &Shared) -> Outcome {
        Outcome::Sum { total: state.total }
    }

    fn writer(state: &Shared) -> Option<Participant> {
        state
            .expected_writer()
            .and_then(|index| Participant::try_from(index).ok())
    }

    fn view(state: &Shared, ensemble: &Ensemble, vp: &Viewport) -> View {
        let participant_count = participant_count(ensemble);
        let target = target_total(participant_count);
        let display_total = state.display_total();

        View::new()
            .header(vp.fit_text(format!(
                "Cumulative sum - total {display_total} of target {target}"
            )))
            .agents(vp.fit_text(render_agents(state, ensemble, participant_count)))
            .state(vp.fit_text(render_state(
                state,
                participant_count,
                display_total,
                target,
                vp,
            )))
            .status_bar(vp.fit_text(format!(
                "{} - total {display_total} / target {target}",
                if state.finalized {
                    "finalized"
                } else {
                    "collecting"
                }
            )))
    }

    fn participant_count(ensemble: &Ensemble) -> usize {
        ensemble.len()
    }

    fn target_total(participant_count: usize) -> u64 {
        // Contributions are drawn modulo 1000; no separate target is stored.
        (participant_count as u64).saturating_mul(1000)
    }

    fn render_agents(_state: &Shared, ensemble: &Ensemble, participant_count: usize) -> String {
        let mut agents = String::new();
        for idx in 0..participant_count {
            let Ok(participant) = Participant::try_from(idx) else {
                continue;
            };
            if let Some(peer) = ensemble.peer_at(participant) {
                let _ = writeln!(agents, "P{idx}: {}", peer.fmt_short());
            } else {
                let _ = writeln!(agents, "P{idx}");
            }
        }
        agents
    }

    fn render_state(
        state: &Shared,
        participant_count: usize,
        display_total: u64,
        target: u64,
        vp: &Viewport,
    ) -> String {
        let mut body = String::from("Contributions\nparticipant  value\n");
        for idx in 0..participant_count {
            let value = state
                .contributions
                .get(idx)
                .and_then(|value| *value)
                .map_or_else(|| "pending".to_string(), |value| value.to_string());
            let _ = writeln!(body, "P{idx:<11} {value}");
        }
        let _ = write!(body, "\nTotal {}", progress_bar(display_total, target, vp));
        body
    }

    fn progress_bar(total: u64, target: u64, vp: &Viewport) -> String {
        if vp.width == 0 {
            return String::new();
        }
        let width = usize::from(vp.width).saturating_sub(16).max(1);
        let clamped = total.min(target);
        let filled = if target == 0 {
            0
        } else {
            ((clamped as u128 * width as u128) / target as u128) as usize
        };
        let empty = width.saturating_sub(filled);
        if vp.color.supports_color() {
            format!(
                "[\x1b[1;32m{}\x1b[0m{}] {total}/{target}",
                "#".repeat(filled),
                ".".repeat(empty)
            )
        } else {
            format!(
                "[{}{}] {total}/{target}",
                "#".repeat(filled),
                ".".repeat(empty)
            )
        }
    }

    fn initialize(shared: &mut Shared, params: Params) -> Result<(), ProgramFault> {
        if params.target_size < 2 {
            return Err(anyhow!("cumulative-sum needs at least two participants").into());
        }
        if u8::try_from(params.target_size).is_err() {
            return Err(anyhow!("participant count must fit in u8").into());
        }

        shared.contributions = vec![None; params.target_size as usize];
        Ok(())
    }

    /// Position-0 boundary: size the contributions buffer to the committed
    /// ensemble, then queue the first contribution when this node owes it.
    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
        ensemble: &arena0::Ensemble,
    ) -> Result<ProgramTransition<CumulativeSum>, ProgramFault> {
        let n = ensemble.len();
        ctx.shared_mut().contributions = vec![None; n];
        queue_contribution_if_due(ctx)?;
        Ok(Transition::Stay)
    }

    fn on_message(
        ctx: &mut Context<Shared, Local>,
        from: Participant,
        msg: Message,
    ) -> MessageApply<CumulativeSum> {
        let Message::Contribute { value } = msg;
        // Unique-writer rule: only the expected writer's message applies;
        // anyone else is a deterministic reject (no sibling candidates).
        if ctx.shared().expected_writer() != Some(from.index()) {
            return Ok(ApplyDecision::Reject);
        }
        let n = ctx.ensemble().len();
        // Key the contribution by the AUTHENTICATED sender, never a value the
        // message could spoof: `from` is resolved from the signed transport peer.
        apply_contribution(ctx.shared_mut(), from, value);
        let transition = finalize_if_ready(ctx.shared_mut(), n);
        if matches!(transition, Transition::Stay) {
            queue_contribution_if_due(ctx).map_err(ProtocolFault::shared_violation)?;
        }
        Ok(ApplyDecision::Accept(transition))
    }

    /// Draw and queue this node's contribution when it owns the next slot.
    ///
    /// The shared slot is filled only when the author's own message is applied
    /// through [`on_message`], so the draw is deterministic per node and the
    /// unique-writer rule holds at every position.
    fn queue_contribution_if_due(ctx: &mut Context<Shared, Local>) -> arena0::anyhow::Result<()> {
        if ctx.shared().expected_writer() != Some(ctx.me().index()) || ctx.local().sent {
            return Ok(());
        }
        let mut buf = [0u8; 8];
        ctx.random(&mut buf);
        let value = u64::from_le_bytes(buf) % 1000;
        ctx.mutate_local(|s| s.sent = true);
        ctx.effects().broadcast(&Message::Contribute { value });
        Ok(())
    }

    fn on_query(_shared: &Shared, _: ()) {}

    /// Once every participant has contributed, write the agreed total to shared
    /// state and end the session.
    fn finalize_if_ready(state: &mut Shared, n: usize) -> ProgramTransition<CumulativeSum> {
        let count = state.contributions.iter().filter(|c| c.is_some()).count();
        if count != n {
            return Transition::Stay;
        }
        let total: u64 = state.contributions.iter().flatten().sum();
        state.total = total;
        state.finalized = true;
        Transition::End
    }

    fn apply_contribution(state: &mut Shared, from: Participant, value: u64) {
        if let Some(slot) = state.contributions.get_mut(from.index()) {
            *slot = Some(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0::testing::{FaultStatus, Harness, TestHarness};
    use arena0::types::{ColorDepth, Slot};

    fn peer_a() -> PeerId {
        PeerId([1u8; 32])
    }

    fn slot(view: &View, slot: Slot) -> &str {
        view.slots.get(&slot).map_or("", String::as_str)
    }

    fn assert_no_sgr(view: &View) {
        for text in view.slots.values() {
            assert!(!text.contains("\x1b["), "mono view contains SGR: {text:?}");
        }
    }

    /// Apply this harness's own queued broadcast through `on_message`, as the
    /// runtime authors its own queued messages.
    fn author_queued(h: &mut TestHarness<CumulativeSum>, effects: &[Effect]) {
        let data = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Broadcast { data } => Some(data.clone()),
                _ => None,
            })
            .expect("session start queued a contribution");
        let msg: Message = borsh::from_slice(&data).expect("contribution decodes");
        let authored = h.message(h.peer_id(), msg);
        assert!(matches!(authored.fault, FaultStatus::None));
    }

    #[arena0::test(
        CumulativeSum,
        Params {
            target_size: 2
        }
    )]
    fn view_renders_contributions_table_and_bar(h: ()) {
        let started = h.session_started(peer_a());
        assert!(matches!(started.fault, FaultStatus::None));
        // The session-start handler queued P0's contribution; the author
        // applies its own message through `on_message`.
        author_queued(&mut h, &started.effects);
        // The remote P1 contribution is then applied at the message boundary.
        let remote = peer_a();
        let contributed = h.message(remote, Message::Contribute { value: 250 });
        assert!(matches!(contributed.fault, FaultStatus::None));

        let view = h.view(Viewport {
            width: 80,
            color: ColorDepth::Ansi16,
        });
        let state = slot(&view, Slot::State);
        assert!(state.contains("Contributions"));
        assert!(state.contains("P0"));
        assert!(state.contains("P1"));
        assert!(state.contains("250"));
        assert!(state.contains("Total ["));
        assert!(slot(&view, Slot::Agents).contains("P0"));
        assert!(slot(&view, Slot::StatusBar).contains("target 2000"));
    }

    #[arena0::test(
        CumulativeSum,
        Params {
            target_size: 2
        }
    )]
    fn view_mono_contains_no_sgr(h: ()) {
        let started = h.session_started(peer_a());
        author_queued(&mut h, &started.effects);
        h.message(peer_a(), Message::Contribute { value: 250 });

        let view = h.view(Viewport {
            width: 80,
            color: ColorDepth::Mono,
        });
        assert_no_sgr(&view);
        assert!(slot(&view, Slot::State).contains("Contributions"));
    }
}
