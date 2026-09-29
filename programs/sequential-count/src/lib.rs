//! N-party sequential counting in a commit-reveal-selected round-robin order.
//!
//! Every participant first commits and reveals a private random nonce. The XOR
//! of all nonces selects the first participant. The program then counts from
//! one to `count_to`, with exactly one authenticated writer per step and the
//! turn moving around the committed ensemble after every count.

use std::fmt::Write;

use arena0::prelude::*;
use arena0_primitives::commit_reveal::{
    self, CommitReveal, CommitRevealAuthorExt, CommitRevealFieldExt, CommitRevealLocal,
    CommitRevealLocalState, MyTurn,
};
use arena0_primitives::turn_manager::TurnManager;

const MAX_PARTICIPANTS: u32 = 32;
const MAX_COUNT: u32 = 4096;

#[arena0::data]
pub struct Params {
    pub target_size: u32,
    pub count_to: u32,
}

#[arena0::message]
pub enum Message {
    CommitReveal(commit_reveal::Message<[u8; 32]>),
    Count { value: u32 },
}

#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Selecting the turn order")]
    Setup,
    #[phase(description = "Counting in round-robin order")]
    Counting,
}

#[arena0::outcome]
pub enum Outcome {
    Counted {
        final_count: u32,
        order: Vec<Participant>,
        history: Vec<Participant>,
    },
}

#[arena0::state(max = 16384)]
pub struct Shared {
    #[phase]
    phase: Phase,
    target_size: u32,
    count_to: u32,
    count: u32,
    order: Vec<Participant>,
    history: Vec<Participant>,
    turns: Option<TurnManager>,
    #[primitive(route = Message::CommitReveal)]
    commit_reveal: CommitReveal<[u8; 32]>,
}

/// Participant-local decision state. It is carried explicitly through local
/// ABI calls and never enters the shared state commitment.
#[arena0::local]
#[derive(Default)]
pub struct Local {
    #[secret]
    commit_reveal: CommitRevealLocal<[u8; 32]>,
    last_sent: Option<u32>,
}

impl CommitRevealLocalState<[u8; 32]> for Local {
    fn commit_reveal_local(&self) -> &CommitRevealLocal<[u8; 32]> {
        &self.commit_reveal
    }

    fn commit_reveal_local_mut(&mut self) -> &mut CommitRevealLocal<[u8; 32]> {
        &mut self.commit_reveal
    }
}

#[arena0::program(
    name = "sequential-count",
    display_name = "Sequential Count Conformance",
    version = "1.0.0",
    description = "N-party sequential counting in a jointly selected round-robin order",
    participants = 3..=32,
    capabilities(auto)
)]
pub mod sequential_count {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;
    type Message = super::Message;
    type Params = super::Params;
    type Outcome = super::Outcome;

    fn outcome(state: &Shared) -> Outcome {
        Outcome::Counted {
            final_count: state.count,
            order: state.order.clone(),
            history: state.history.clone(),
        }
    }

    fn writer(state: &Shared) -> Option<Participant> {
        match state.phase() {
            Phase::Setup => state.commit_reveal.expected_writer(),
            Phase::Counting => state.turns.as_ref().map(TurnManager::current),
        }
    }

    fn view(state: &Shared, _ensemble: &Ensemble, vp: &Viewport) -> View {
        View::new()
            .header(vp.fit_text(format!(
                "Sequential count - {} of {}",
                state.count, state.count_to
            )))
            .agents(vp.fit_text(render_agents(state)))
            .state(vp.fit_text(render_state(state)))
            .status_bar(vp.fit_text(format!(
                "{} - {} participants",
                phase_label(state.phase()),
                state.target_size
            )))
    }

    fn render_agents(state: &Shared) -> String {
        let current = state.turns.as_ref().map(TurnManager::current);
        let mut agents = String::new();
        if state.order.is_empty() {
            for index in 0..state.target_size {
                let _ = writeln!(agents, "P{index}: awaiting order");
            }
        } else {
            for participant in &state.order {
                let marker = if Some(*participant) == current {
                    " (current)"
                } else {
                    ""
                };
                let _ = writeln!(agents, "P{}{}", participant.index(), marker);
            }
        }
        agents
    }

    fn render_state(state: &Shared) -> String {
        if state.phase() == Phase::Setup {
            return "Selecting the starting participant with commit-reveal".to_owned();
        }

        let mut body = format!("Count: {} / {}", state.count, state.count_to);
        if !state.history.is_empty() {
            body.push_str("\nRecent writers:");
            let start = state.history.len().saturating_sub(8);
            for participant in &state.history[start..] {
                let _ = write!(body, " P{}", participant.index());
            }
        }
        body
    }

    fn phase_label(phase: Phase) -> &'static str {
        match phase {
            Phase::Setup => "selecting order",
            Phase::Counting => "counting",
        }
    }

    fn initialize(shared: &mut Shared, params: Params) -> Result<(), ProgramFault> {
        if params.target_size < 3 {
            return Err(anyhow!("sequential-count needs at least three participants").into());
        }
        if params.target_size > MAX_PARTICIPANTS {
            return Err(anyhow!(
                "sequential-count supports at most {MAX_PARTICIPANTS} participants"
            )
            .into());
        }
        if params.count_to == 0 {
            return Err(anyhow!("count_to must be greater than zero").into());
        }
        if params.count_to > MAX_COUNT {
            return Err(anyhow!("count_to must not exceed {MAX_COUNT}").into());
        }
        shared.target_size = params.target_size;
        shared.count_to = params.count_to;
        Ok(())
    }

    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
        ensemble: &Ensemble,
    ) -> Result<ProgramTransition<SequentialCount>, ProgramFault> {
        if ensemble.len() != ctx.shared().target_size as usize {
            return Err(anyhow!(
                "expected {} participants, got {}",
                ctx.shared().target_size,
                ensemble.len()
            )
            .into());
        }
        let configured = ctx
            .shared_mut()
            .commit_reveal
            .set_participant_count(ensemble.len());
        configured.map_err(|error| anyhow!(error))?;
        queue_setup_action(ctx)?;
        Ok(Transition::Stay)
    }

    fn on_message(
        ctx: &mut Context<Shared, Local>,
        from: Participant,
        message: Message,
    ) -> MessageApply<SequentialCount> {
        match message {
            Message::CommitReveal(message) => {
                if ctx.shared().phase() != Phase::Setup {
                    return Ok(ApplyDecision::Reject);
                }
                // Unique-writer rule (Setup phase): only the participant whose
                // commit or reveal is next may write; any other sender is a
                // deterministic reject (no sibling candidates at one position).
                if !ctx.shared().commit_reveal.is_writer(from) {
                    return Ok(ApplyDecision::Reject);
                }
                if ctx.commit_reveal().handle(from, message).is_err() {
                    return Ok(ApplyDecision::Reject);
                }
                if !ctx.shared().commit_reveal.is_complete() {
                    queue_setup_action(ctx).map_err(ProtocolFault::shared_violation)?;
                    return Ok(ApplyDecision::Accept(Transition::Stay));
                }

                apply_order(ctx.shared_mut())?;
                queue_count_if_due(ctx).map_err(ProtocolFault::shared_violation)?;
                Ok(ApplyDecision::Accept(Transition::To(Phase::Counting)))
            }
            Message::Count { value } => {
                if ctx.shared().phase() != Phase::Counting {
                    return Ok(ApplyDecision::Reject);
                }
                let expected_writer = ctx
                    .shared()
                    .turns
                    .as_ref()
                    .expect("turn order initialized")
                    .current();
                if from != expected_writer {
                    return Ok(ApplyDecision::Reject);
                }
                let expected_value = ctx.shared().count + 1;
                if value != expected_value {
                    return Ok(ApplyDecision::Reject);
                }

                let finished = apply_count(ctx.shared_mut(), from, value);
                if !finished {
                    queue_count_if_due(ctx).map_err(ProtocolFault::shared_violation)?;
                }
                if finished {
                    Ok(ApplyDecision::Accept(Transition::End))
                } else {
                    Ok(ApplyDecision::Accept(Transition::Stay))
                }
            }
        }
    }

    fn on_query(_shared: &Shared, _: ()) {}

    /// Queue this node's next commit or reveal when it owns the setup writer.
    fn queue_setup_action(ctx: &mut Context<Shared, Local>) -> arena0::anyhow::Result<()> {
        match ctx.commit_reveal().my_turn() {
            Some(MyTurn::Reveal(reveal)) => reveal.broadcast(&mut ctx.effects()),
            Some(MyTurn::Commit) => {
                let mut nonce = [0u8; 32];
                ctx.random(&mut nonce);
                ctx.commit_reveal()
                    .commit(nonce)?
                    .broadcast(&mut ctx.effects());
            }
            None => {}
        }
        Ok(())
    }

    /// Queue this node's next count when the turn order selects it.
    fn queue_count_if_due(ctx: &mut Context<Shared, Local>) -> arena0::anyhow::Result<()> {
        let next = ctx.shared().count + 1;
        let is_my_turn = ctx
            .shared()
            .turns
            .as_ref()
            .is_some_and(|turns| turns.current() == ctx.me());
        if is_my_turn && ctx.local().last_sent != Some(next) {
            ctx.mutate_local(|state| state.last_sent = Some(next));
            ctx.effects().broadcast(&Message::Count { value: next });
        }
        Ok(())
    }

    /// Install the deterministic round-robin order selected by the completed
    /// nonce exchange. Both the producer's reveal reaction and receivers call
    /// this helper before entering the counting phase.
    fn apply_order(state: &mut Shared) -> Result<(), arena0::anyhow::Error> {
        let participant_count = state.commit_reveal.participant_count();
        let start = state
            .commit_reveal
            .random_range(participant_count as u64)
            .ok_or_else(|| anyhow!("completed commit-reveal has random bytes"))?
            as usize;
        let order: Vec<Participant> = (0..participant_count)
            .map(|offset| {
                Participant::try_from((start + offset) % participant_count)
                    .expect("commit-reveal participant count fits in u8")
            })
            .collect();
        state.order.clone_from(&order);
        state.turns = Some(TurnManager::new(order));
        Ok(())
    }

    fn apply_count(state: &mut Shared, from: Participant, value: u32) -> bool {
        state.count = value;
        state.history.push(from);
        state
            .turns
            .as_mut()
            .expect("turn order initialized")
            .advance();
        state.count == state.count_to
    }
}
