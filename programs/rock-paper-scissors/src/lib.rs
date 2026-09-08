use std::fmt::Write;

use arena0::prelude::*;
use arena0_primitives::commit_reveal::{
    self, CommitReveal, CommitRevealLocal, CommitRevealLocalFieldExt, CommitRevealLocalState,
    CommitRevealSharedFieldExt,
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

#[arena0::pending]
pub enum Pending {
    ChoosingMove,
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

    fn view(ctx: &SharedContext, vp: &Viewport) -> View {
        let state = ctx.shared();
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

    /// Position-0 boundary: seed the match. Shared handler, so it issues no
    /// callout and broadcasts nothing; asking the agent for a move is `on_react`'s
    /// job. The commit-reveal primitive starts from its `Default`.
    fn on_session_started(ctx: &mut SharedContext) -> Result<Transition<Phase>, ProgramFault> {
        ctx.mutate_shared(|s| {
            s.total_rounds = 3;
            s.round = 1;
        });
        Ok(Transition::To(Phase::Playing))
    }

    /// Local decision hook. Broadcasts the owed reveal once every commit is in,
    /// otherwise asks the agent for this round's move when one is still owed.
    fn on_react(ctx: &mut Context) -> Result<(), ProgramFault> {
        // Unique-writer rule: react only when this node is the participant
        // whose action is next (first missing commit, then first missing
        // reveal). An idle node never broadcasts into a position it cannot
        // win, so no sibling candidates converge on one position.
        if ctx.shared().commit_reveal.expected_writer() != Some(ctx.me()) {
            return Ok(());
        }
        if let Some(reveal) = ctx.commit_reveal().take_reveal() {
            reveal.broadcast();
            return Ok(());
        }
        if ctx.commit_reveal().needs_commit() {
            let slot = ctx.me().index();
            let req = ctx.shared().choice_request(slot);
            ctx.effects()
                .callout(req)
                .pending(Pending::ChoosingMove)
                .dispatch();
        }
        Ok(())
    }

    fn on_input(ctx: &mut Context, input: Input) -> Result<(), InputFault> {
        let Input::ChooseMove(choice) = input;
        ctx.commit_reveal().commit(choice)?.broadcast();
        Ok(())
    }

    fn on_message(
        ctx: &mut SharedContext,
        from: Participant,
        msg: Message,
    ) -> Result<ApplyDecision<Phase>, ProtocolFault> {
        let Message::CommitReveal(msg) = msg;
        // Unique-writer rule: only the participant whose action is next (the
        // first missing commit, then the first missing reveal) may write;
        // any other sender is a deterministic reject.
        if ctx.shared().commit_reveal.expected_writer() != Some(from) {
            return Ok(ApplyDecision::Reject);
        }
        if ctx.commit_reveal().handle(from, msg).is_err() {
            return Ok(ApplyDecision::Reject);
        }
        if !ctx.shared().commit_reveal.is_complete() {
            return Ok(ApplyDecision::Accept(Transition::Stay));
        }

        // Score in ABSOLUTE participant order so every node computes the identical
        // shared transition (no local-perspective slot).
        let finished = ctx.mutate_shared(|s| {
            if let Some(vals) = s.commit_reveal.values() {
                let p0 = *vals[0];
                let p1 = *vals[1];
                if p0.beats(p1) {
                    s.scores[0] += 1;
                } else if p1.beats(p0) {
                    s.scores[1] += 1;
                }
            }

            let needed = (s.total_rounds as u32 / 2) + 1;
            let finished =
                s.round >= s.total_rounds || s.scores[0] >= needed || s.scores[1] >= needed;

            if !finished {
                s.round += 1;
                s.commit_reveal
                    .reset()
                    .expect("completed commit-reveal round can reset");
            }
            finished
        });

        if finished {
            return Ok(ApplyDecision::Accept(Transition::End));
        }
        // The next round's callout is issued by `on_react` (needs_commit after reset).
        Ok(ApplyDecision::Accept(Transition::Stay))
    }

    fn on_query(_ctx: &SharedContext, _: ()) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0::testing::{ALICE, BOB, DeliverySchedule, FaultStatus, Harness, Scenario};
    use arena0::types::{ColorDepth, Slot};
    use arena0_primitives::commit_reveal;

    fn peer_a() -> PeerId {
        PeerId([1u8; 32])
    }

    fn make_commit(choice: Choice) -> Message {
        let proto = CommitReveal::<Choice>::default();
        let mut local = CommitRevealLocal::default();
        Message::CommitReveal(
            proto
                .commit_with_salt(&mut local, choice, [0u8; 32])
                .expect("fresh commit"),
        )
    }

    fn make_reveal(choice: Choice) -> Message {
        Message::CommitReveal(commit_reveal::Message::Reveal {
            value: choice,
            salt: [0u8; 32],
        })
    }

    /// Play one full round on a single native replica by hand-delivering every
    /// broadcast, self-delivery included (the runtime does this automatically at
    /// canonical positions). `me` chooses `mine`; the opponent chose `theirs`.
    /// Returns the result of the reveal that completes the round.
    fn play_round<H>(h: &mut H, mine: Choice, theirs: Choice) -> arena0::testing::HandlerResult
    where
        H: Harness<RockPaperScissors>,
    {
        // Local commit (answers the pending ChooseMove callout), broadcast.
        let fx = h.resolve_callout::<callouts::ChooseMove>(mine);
        let my_commit = fx.messages::<Message>().remove(0);
        // Apply my own commit, then the opponent's; the second commit makes the
        // reveal due, which `on_react` broadcasts.
        h.message(h.peer_id(), my_commit);
        let fx = h.message(peer_a(), make_commit(theirs));
        let my_reveal = fx.messages::<Message>().remove(0);
        // Apply my reveal, then the opponent's; the second completes the round.
        h.message(h.peer_id(), my_reveal);
        h.message(peer_a(), make_reveal(theirs))
    }

    fn slot(view: &View, slot: Slot) -> &str {
        view.slots.get(&slot).map_or("", String::as_str)
    }

    fn assert_no_sgr(view: &View) {
        for text in view.slots.values() {
            assert!(!text.contains("\x1b["), "mono view contains SGR: {text:?}");
        }
    }

    #[arena0::test(RockPaperScissors, ())]
    fn initial_state(h: ()) {
        let state = h.shared();
        assert_eq!(state.phase(), Phase::Setup);
        assert_eq!(state.round, 0);
        assert_eq!(state.scores, [0, 0]);
    }

    #[arena0::test(RockPaperScissors, ())]
    fn session_started_transitions_to_playing(h: ()) {
        let fx = h.session_started(peer_a());
        assert!(matches!(fx.fault, FaultStatus::None));
        let state = h.shared();
        assert_eq!(state.phase(), Phase::Playing);
        assert_eq!(state.round, 1);
        assert_eq!(state.total_rounds, 3);
        assert_eq!(state.scores, [0, 0]);
        assert!(fx.has_callout());
        let callout = fx.expect_callout::<RockPaperScissors, callouts::ChooseMove>();
        assert_eq!(callout.request.round, 1);
        assert_eq!(callout.request.total_rounds, 3);
        assert_eq!(callout.request.your_score, 0);
        assert_eq!(callout.request.their_score, 0);
        assert_eq!(
            callout.pending_label.as_deref(),
            Some(Pending::ChoosingMove.as_str())
        );
        assert_eq!(
            callout.expected_type.as_deref(),
            Some(std::any::type_name::<Choice>())
        );
    }

    #[arena0::test(RockPaperScissors, ())]
    fn choice_input_broadcasts_commit(h: ()) {
        h.session_started(peer_a());
        let fx = h.resolve_callout::<callouts::ChooseMove>(Choice::Rock);
        assert!(matches!(fx.fault, FaultStatus::None));
        assert!(fx.has_broadcast());
        let msgs = fx.messages::<Message>();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            msgs[0],
            Message::CommitReveal(commit_reveal::Message::Commit(_))
        ));
    }

    #[arena0::test(RockPaperScissors, ())]
    fn reveal_is_broadcast_by_react_once_every_commit_is_applied(h: ()) {
        h.session_started(peer_a());
        // The local peer (slot 0) is the expected writer in the commit phase:
        // its callout is pending from session start. Answer it (stashes the
        // private value and broadcasts the local commit), then apply both
        // commits in writer order; the final react broadcasts the reveal.
        let local = h.peer_id();
        let fx = h.resolve_callout::<callouts::ChooseMove>(Choice::Rock);
        let msgs = fx.messages::<Message>();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            msgs[0],
            Message::CommitReveal(commit_reveal::Message::Commit(_))
        ));

        // Apply the local commit (slot 0), then peer_a's (slot 1): the round
        // completes and react broadcasts the owed reveal.
        h.message(local, msgs[0].clone());
        let fx = h.message(peer_a(), make_commit(Choice::Scissors));
        let reveals = fx.messages::<Message>();
        assert_eq!(reveals.len(), 1, "reveals: {reveals:?}");
        assert!(matches!(
            reveals[0],
            Message::CommitReveal(commit_reveal::Message::Reveal { .. })
        ));
    }

    #[test]
    fn choice_helpers() {
        assert!(Choice::Rock.beats(Choice::Scissors));
        assert!(Choice::Scissors.beats(Choice::Paper));
        assert!(Choice::Paper.beats(Choice::Rock));
        assert!(!Choice::Rock.beats(Choice::Rock));
        assert!(!Choice::Rock.beats(Choice::Paper));
    }

    #[arena0::test(RockPaperScissors, ())]
    fn full_round_local_wins(h: ()) {
        h.session_started(peer_a());

        let fx = play_round(&mut h, Choice::Rock, Choice::Scissors);

        let state = h.shared();
        assert_eq!(state.round, 2, "should advance to round 2");
        assert_eq!(state.scores[0], 1, "local should score for rock > scissors");
        assert_eq!(state.scores[1], 0);
        assert!(fx.has_callout(), "should request next choice");
    }

    #[arena0::test(RockPaperScissors, ())]
    fn full_round_draw(h: ()) {
        h.session_started(peer_a());

        play_round(&mut h, Choice::Paper, Choice::Paper);

        let state = h.shared();
        assert_eq!(state.scores, [0, 0], "draw should not change scores");
        assert_eq!(state.round, 2);
    }

    #[arena0::test(RockPaperScissors, ())]
    fn view_renders_revealed_hands_and_scores(h: ()) {
        h.session_started(peer_a());

        // Two clinching rounds: local (rock) beats opponent (scissors) 2-0, so the
        // game ends with the final round's commit-reveal still Complete (unreset),
        // which is what surfaces the revealed hands in the view.
        play_round(&mut h, Choice::Rock, Choice::Scissors);
        play_round(&mut h, Choice::Rock, Choice::Scissors);
        assert_eq!(h.shared().scores, [2, 0]);

        let view = h.view(Viewport {
            width: 80,
            color: ColorDepth::Ansi16,
        });
        assert!(slot(&view, Slot::State).contains("Rock"));
        assert!(slot(&view, Slot::State).contains("Scissors"));
        assert!(slot(&view, Slot::Agents).contains("Rock"));
        assert!(slot(&view, Slot::StatusBar).contains("playing"));
    }

    #[arena0::test(RockPaperScissors, ())]
    fn view_mono_contains_no_sgr(h: ()) {
        h.session_started(peer_a());
        // Drive to the Revealing phase (both commits applied, reveals pending) so
        // the view renders sealed hands.
        let fx = h.resolve_callout::<callouts::ChooseMove>(Choice::Rock);
        let my_commit = fx.messages::<Message>().remove(0);
        h.message(h.peer_id(), my_commit);
        h.message(peer_a(), make_commit(Choice::Scissors));

        let view = h.view(Viewport {
            width: 80,
            color: ColorDepth::Mono,
        });
        assert_no_sgr(&view);
        assert!(slot(&view, Slot::State).contains("[sealed]"));
    }

    #[arena0::test(RockPaperScissors, ())]
    fn clinch_ends_game_early(h: ()) {
        h.session_started(peer_a());

        play_round(&mut h, Choice::Rock, Choice::Scissors);
        assert_eq!(h.shared().round, 2);

        let fx = play_round(&mut h, Choice::Rock, Choice::Scissors);

        let state = h.shared();
        assert_eq!(state.scores[0], 2);
        assert_eq!(state.scores[1], 0);
        assert!(fx.has_session_end());
    }

    #[test]
    fn bilateral_pair_scenario_converges_and_finishes() {
        let run = Scenario::<RockPaperScissors>::named("alice wins best of three")
            .input(ALICE, Input::ChooseMove(Choice::Rock))
            .input(BOB, Input::ChooseMove(Choice::Scissors))
            .deliver_all()
            .snapshot("round-one")
            .input(ALICE, Input::ChooseMove(Choice::Paper))
            .input(BOB, Input::ChooseMove(Choice::Rock))
            .deliver_all()
            .snapshot("finished")
            .run_with_snapshots(());
        let pair = run.pair();
        let trace = pair.trace();
        trace.assert_shared_aligned();
        trace.assert_replayable();
        assert!(
            run.snapshot("round-one")
                .expect("round-one")
                .transcript
                .contains("MessageReceived")
        );
        assert!(
            run.snapshot("finished")
                .expect("finished")
                .transcript
                .contains("SessionEnd")
        );
    }

    #[test]
    fn generated_delivery_schedule_property_covers_rock_paper_scissors() {
        let mut coverage = arena0::testing::CoverageReport::default();
        for seed in 0..8 {
            let schedule = DeliverySchedule::generated(seed, 4).without_drops();
            let pair = Scenario::<RockPaperScissors>::named(format!("generated schedule {seed}"))
                .input(ALICE, Input::ChooseMove(Choice::Rock))
                .input(BOB, Input::ChooseMove(Choice::Scissors))
                .delivery_schedule(schedule)
                .deliver_all()
                .input(ALICE, Input::ChooseMove(Choice::Paper))
                .input(BOB, Input::ChooseMove(Choice::Rock))
                .deliver_all()
                .run(());

            pair.trace().assert_shared_aligned();
            pair.trace().assert_replayable();
            coverage = pair.coverage();
            coverage.assert_event("MessageReceived");
        }
        coverage.assert_event_effect("MessageReceived", "SessionEnd");
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
