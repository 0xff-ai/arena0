use std::fmt::Write;

use arena0::prelude::*;
use arena0_primitives::commit_reveal::{
    self, CommitReveal, CommitRevealAuthorExt, CommitRevealFieldExt, CommitRevealLocal,
    CommitRevealLocalState, MyTurn,
};

#[arena0::data]
#[derive(Default, Copy)]
pub enum Choice {
    #[default]
    Rock,
    Paper,
    Scissors,
}

impl Choice {
    fn beats(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Rock, Self::Scissors)
                | (Self::Scissors, Self::Paper)
                | (Self::Paper, Self::Rock)
        )
    }

    fn label(self) -> &'static str {
        match self {
            Self::Rock => "Rock",
            Self::Paper => "Paper",
            Self::Scissors => "Scissors",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Rock => "✊",
            Self::Paper => "✋",
            Self::Scissors => "✌",
        }
    }

    fn sgr(self) -> &'static str {
        match self {
            Self::Rock => "\x1b[1;37m",
            Self::Paper => "\x1b[1;36m",
            Self::Scissors => "\x1b[1;35m",
        }
    }
}

#[arena0::callouts]
pub enum Callout {
    /// Choose rock, paper, or scissors
    #[arena0::callout(output = Choice)]
    ChooseMove {
        round: u8,
        total_rounds: u8,
        your_score: u32,
        their_score: u32,
    },
}

#[arena0::message]
pub enum Message {
    CommitReveal(commit_reveal::Message<Choice>),
}

// No terminal phase: in the outcome contract the session ends via
// `Transition::End` (which derives the outcome and emits `SessionEnd`), not by
// moving to a "finished" phase. The terminal signal is session-end + the
// derived `Outcome` receipt, so `Playing` is the last program phase.
#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Waiting for opponent")]
    Setup,
    #[phase(description = "Game in progress")]
    Playing,
}

/// Derived terminal receipt: a pure projection from final shared state.
///
/// Computed in absolute participant order (`scores[0]` is participant 0), so
/// every party derives the identical outcome from the agreed final state.
#[arena0::outcome]
pub enum Outcome {
    Win {
        winner: Participant,
        scores: [u32; 2],
    },
    Draw {
        scores: [u32; 2],
    },
}

#[arena0::state(max = 8192)]
pub struct Shared {
    #[phase]
    phase: Phase,
    round: u8,
    total_rounds: u8,
    scores: [u32; 2],
    #[primitive(route = Message::CommitReveal)]
    commit_reveal: CommitReveal<Choice>,
}

/// Participant-local commit-reveal stash. It is carried explicitly through
/// local ABI calls and excluded from the shared state commitment.
#[arena0::local]
#[derive(Default)]
pub struct Local {
    #[secret]
    commit_reveal: CommitRevealLocal<Choice>,
}

impl CommitRevealLocalState<Choice> for Local {
    fn commit_reveal_local(&self) -> &CommitRevealLocal<Choice> {
        &self.commit_reveal
    }

    fn commit_reveal_local_mut(&mut self) -> &mut CommitRevealLocal<Choice> {
        &mut self.commit_reveal
    }
}

impl Shared {
    fn choice_request(&self, slot: usize) -> callouts::ChooseMove {
        callouts::ChooseMove {
            round: self.round,
            total_rounds: self.total_rounds,
            your_score: self.scores[slot],
            their_score: self.scores[1 - slot],
        }
    }
}

#[arena0::program(
    name = "rock-paper-scissors",
    display_name = "Rock-Paper-Scissors",
    version = "1.0.0",
    description = "Best-of-3 rock-paper-scissors with commit-reveal",
    participants = 2,
    capabilities(auto)
)]
pub mod rock_paper_scissors {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;
    type Message = super::Message;
    type Callout = super::Callout;
    type Input = super::Input;
    type Outcome = super::Outcome;

    /// Pure projection from final shared state; no context, effects, or entropy.
    fn outcome(state: &Shared) -> Outcome {
        let [p0, p1] = state.scores;
        if p0 > p1 {
            Outcome::Win {
                winner: Participant::new(0),
                scores: state.scores,
            }
        } else if p1 > p0 {
            Outcome::Win {
                winner: Participant::new(1),
                scores: state.scores,
            }
        } else {
            Outcome::Draw {
                scores: state.scores,
            }
        }
    }

    fn writer(state: &Shared) -> Option<Participant> {
        state.commit_reveal.expected_writer()
    }

    fn view(state: &Shared, _ensemble: &Ensemble, vp: &Viewport) -> View {
        let header = vp.fit_text(format!(
            "Rock-paper-scissors - round {} of {}",
            state.round, state.total_rounds
        ));
        let agents = vp.fit_text(render_agents(state, None, vp));
        let state_slot = vp.fit_text(render_state(state, None, vp));
        let status_bar = vp.fit_text(format!(
            "{} - round {} of {}",
            phase_label(state.phase()),
            state.round,
            state.total_rounds
        ));

        View::new()
            .header(header)
            .agents(agents)
            .state(state_slot)
            .status_bar(status_bar)
    }

    fn render_agents(state: &Shared, me: Option<usize>, vp: &Viewport) -> String {
        let mut agents = String::new();
        for idx in player_order(me) {
            let hand = state
                .commit_reveal
                .value_at(idx)
                .map(|choice| format!(" {}", render_choice(*choice, vp)))
                .unwrap_or_default();
            let _ = writeln!(
                agents,
                "{}: {} point{}{}",
                player_label(idx, me),
                state.scores[idx],
                if state.scores[idx] == 1 { "" } else { "s" },
                hand
            );
        }
        agents
    }

    fn render_state(state: &Shared, me: Option<usize>, vp: &Viewport) -> String {
        let mut body = String::new();
        if let Some(values) = state.commit_reveal.values() {
            for idx in player_order(me) {
                let _ = writeln!(
                    body,
                    "{} throws {}",
                    player_label(idx, me),
                    render_choice(*values[idx], vp)
                );
            }
        } else if state.commit_reveal.phase() == commit_reveal::Phase::Revealing {
            for idx in player_order(me) {
                let _ = writeln!(body, "{} [sealed]", player_label(idx, me));
            }
        } else {
            body.push_str("Waiting for choices");
        }
        body
    }

    fn render_choice(choice: Choice, vp: &Viewport) -> String {
        let text = format!("{} {}", choice.glyph(), choice.label());
        if vp.color.supports_color() {
            format!("{}{text}\x1b[0m", choice.sgr())
        } else {
            text
        }
    }

    fn player_order(me: Option<usize>) -> [usize; 2] {
        match me {
            Some(1) => [1, 0],
            _ => [0, 1],
        }
    }

    fn player_label(idx: usize, me: Option<usize>) -> String {
        // Role first, seat index second: the human always reads "you"/"opponent"
        // first no matter which seat they hold (L047).
        match me {
            Some(me) if idx == me => format!("you (P{idx})"),
            Some(_) => format!("opponent (P{idx})"),
            None => format!("P{idx}"),
        }
    }

    fn phase_label(phase: Phase) -> &'static str {
        match phase {
            Phase::Setup => "setup",
            Phase::Playing => "playing",
        }
    }

    /// Position-0 boundary: seed the match. The session-start handler broadcasts nothing.
    /// The resulting state determines the first question. The commit-reveal primitive starts from its `Default`.
    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<ProgramTransition<RockPaperScissors>, ProgramFault> {
        ctx.mutate_shared(|s| {
            s.total_rounds = 3;
            s.round = 1;
        });
        Ok(Transition::To(Phase::Playing))
    }

    fn callout(ctx: &CalloutContext<Shared, Local>) -> Option<Callout> {
        (ctx.shared().commit_reveal.is_writer(ctx.me())
            && ctx
                .shared()
                .commit_reveal
                .needs_commit(&ctx.local().commit_reveal))
        .then(|| ctx.shared().choice_request(ctx.me().index()).into())
    }

    fn on_input(ctx: &mut LocalContext<Shared, Local>, input: Input) -> arena0::anyhow::Result<()> {
        if !ctx.shared().commit_reveal.is_writer(ctx.me()) {
            return Err(anyhow!("this participant does not own the next choice"));
        }
        let Input::ChooseMove(choice) = input;
        ctx.commit_reveal()
            .commit(choice)?
            .broadcast(&mut ctx.effects())?;
        Ok(())
    }

    fn on_message(
        ctx: &mut Context<Shared, Local>,
        from: Participant,
        msg: Message,
    ) -> MessageApply<RockPaperScissors> {
        let Message::CommitReveal(msg) = msg;
        // Unique-writer rule: only the participant whose action is next (the
        // first missing commit, then the first missing reveal) may write;
        // any other sender is a deterministic reject.
        if !ctx.shared().commit_reveal.is_writer(from) {
            return Ok(ApplyDecision::Reject);
        }
        if ctx.commit_reveal().handle(from, msg).is_err() {
            return Ok(ApplyDecision::Reject);
        }
        if !ctx.shared().commit_reveal.is_complete() {
            queue_reveal_if_due(ctx);
            return Ok(ApplyDecision::Accept(Transition::Stay));
        }

        // Score in ABSOLUTE participant order so every node computes the identical
        // state update (no local-perspective slot).
        let finished = apply_completed_round(ctx.shared_mut());

        if finished {
            return Ok(ApplyDecision::Accept(Transition::End));
        }
        // The reset state determines the next round's callout.
        Ok(ApplyDecision::Accept(Transition::Stay))
    }

    /// Queue the owed reveal once every commit is in, when this node owns the
    /// next writer position.
    fn queue_reveal_if_due(ctx: &mut Context<Shared, Local>) {
        if let Some(MyTurn::Reveal(reveal)) = ctx.commit_reveal().my_turn() {
            reveal.broadcast(&mut ctx.effects());
        }
    }

    fn on_query(_shared: &Shared, _: ()) {}

    /// Score a completed reveal round and either reset the protocol for another
    /// round or leave the final reveal visible while ending the session.
    fn apply_completed_round(state: &mut Shared) -> bool {
        let Some(vals) = state.commit_reveal.values() else {
            return false;
        };
        let p0 = *vals[0];
        let p1 = *vals[1];
        if p0.beats(p1) {
            state.scores[0] += 1;
        } else if p1.beats(p0) {
            state.scores[1] += 1;
        }

        let needed = (state.total_rounds as u32 / 2) + 1;
        let finished = state.round >= state.total_rounds
            || state.scores[0] >= needed
            || state.scores[1] >= needed;
        if !finished {
            state.round += 1;
            state
                .commit_reveal
                .reset()
                .expect("completed commit-reveal round can reset");
        }
        finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0::types::{ColorDepth, Slot};

    #[test]
    fn terminal_view_renders_revealed_hands_and_scores() {
        // Finished best-of-3 clinched 2-0: drive a completed Rock-vs-Scissors
        // commit/reveal through the real primitive with fixed public salts, so
        // the final reveal stays visible exactly as it does at session end.
        let mut commit_reveal = CommitReveal::default();
        let mut locals = [CommitRevealLocal::default(), CommitRevealLocal::default()];
        let choices = [Choice::Rock, Choice::Scissors];
        let salts = [[0x11u8; 32], [0x22u8; 32]];
        let commits: Vec<_> = choices
            .iter()
            .zip(locals.iter_mut())
            .zip(salts.iter())
            .map(|((choice, local), salt)| {
                commit_reveal
                    .commit_with_salt(local, *choice, *salt)
                    .expect("fresh commit")
            })
            .collect();
        for (index, commit) in commits.into_iter().enumerate() {
            commit_reveal
                .handle(
                    Participant::try_from(index).expect("test participant fits"),
                    commit,
                )
                .expect("commit applies");
        }
        let reveals: Vec<_> = locals
            .iter_mut()
            .map(|local| commit_reveal.take_reveal(local).expect("reveal is due"))
            .collect();
        for (index, reveal) in reveals.into_iter().enumerate() {
            commit_reveal
                .handle(
                    Participant::try_from(index).expect("test participant fits"),
                    reveal,
                )
                .expect("reveal applies");
        }
        assert!(commit_reveal.is_complete());

        let state = Shared {
            round: 2,
            total_rounds: 3,
            scores: [2, 0],
            commit_reveal,
            ..Shared::default()
        };
        let ensemble = Ensemble::from_peers(vec![PeerId([0; 32]), PeerId([1; 32])])
            .expect("valid view ensemble");
        for color in [ColorDepth::Mono, ColorDepth::Ansi16] {
            let view = <rock_paper_scissors::RockPaperScissors as ProgramView>::view(
                &state,
                &ensemble,
                &Viewport { width: 120, color },
            );
            assert_eq!(view.slots.len(), 4, "{color:?} fills every slot");
            for slot in [Slot::Header, Slot::Agents, Slot::State, Slot::StatusBar] {
                assert!(
                    view.slots.contains_key(&slot),
                    "{color:?} is missing {slot:?}"
                );
            }
            let agents = &view.slots[&Slot::Agents];
            let state_slot = &view.slots[&Slot::State];
            for (participant, score, hand) in [(0, 2, "Rock"), (1, 0, "Scissors")] {
                assert!(
                    state_slot.lines().any(|line| {
                        line.starts_with(&format!("P{participant} throws ")) && line.contains(hand)
                    }),
                    "state associates P{participant} with {hand}: {state_slot:?}"
                );
                assert!(
                    agents.lines().any(|line| {
                        line.starts_with(&format!("P{participant}: {score} points "))
                            && line.contains(hand)
                    }),
                    "agents associate P{participant} with score {score} and {hand}: {agents:?}"
                );
            }
            if color == ColorDepth::Mono {
                assert!(
                    view.slots.values().all(|text| !text.contains("\x1b[")),
                    "mono view must not contain escapes"
                );
            }
        }

        let outcome = <rock_paper_scissors::RockPaperScissors as Program>::outcome(&state);
        assert!(
            matches!(outcome, Outcome::Win { winner, scores: [2, 0] } if winner == Participant::new(0)),
            "P0 wins 2-0"
        );
    }

    #[test]
    fn choice_helpers() {
        assert!(Choice::Rock.beats(Choice::Scissors));
        assert!(Choice::Scissors.beats(Choice::Paper));
        assert!(Choice::Paper.beats(Choice::Rock));
        assert!(!Choice::Rock.beats(Choice::Rock));
        assert!(!Choice::Rock.beats(Choice::Paper));
    }

    #[test]
    fn callout_schema_metadata() {
        let schemas = <Callout as Arena0Callout>::schemas();
        assert_eq!(schemas.len(), 1);

        let s = &schemas[0];
        assert_eq!(s.name, "ChooseMove");
        assert!(!s.prompt.is_empty());
        assert_eq!(
            s.output.as_value()["enum"]
                .as_array()
                .expect("expected enum output schema")
                .len(),
            3
        );
        assert_eq!(
            s.input.as_value()["properties"]
                .as_object()
                .expect("expected object input schema")
                .len(),
            4
        );
    }

    #[test]
    fn commit_reveal_declares_messaging_capability() {
        let capabilities = <Shared as SharedState>::__required_capabilities();

        assert!(capabilities.contains(&Capability::Messaging));
    }
}
