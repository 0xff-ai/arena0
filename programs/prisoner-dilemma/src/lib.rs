use std::fmt::Write;

use arena0::prelude::*;
use arena0_primitives::commit_reveal::{
    self, CommitReveal, CommitRevealLocal, CommitRevealLocalFieldExt, CommitRevealLocalState,
    CommitRevealSharedFieldExt,
};

#[arena0::data]
#[derive(Default, Copy, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum Choice {
    #[default]
    Cooperate,
    Defect,
}

impl Choice {
    fn glyph(self) -> &'static str {
        match self {
            Self::Cooperate => "C",
            Self::Defect => "D",
        }
    }

    fn sgr(self) -> &'static str {
        match self {
            Self::Cooperate => "\x1b[1;32m",
            Self::Defect => "\x1b[1;31m",
        }
    }
}

fn payoff(mine: Choice, theirs: Choice) -> (u32, u32) {
    match (mine, theirs) {
        (Choice::Cooperate, Choice::Cooperate) => (3, 3),
        (Choice::Cooperate, Choice::Defect) => (0, 5),
        (Choice::Defect, Choice::Cooperate) => (5, 0),
        (Choice::Defect, Choice::Defect) => (1, 1),
    }
}

#[arena0::message]
pub enum Message {
    CommitReveal(commit_reveal::Message<Choice>),
}

#[arena0::callouts]
pub enum Callout {
    /// Choose to cooperate or defect
    #[arena0::callout(output = Choice)]
    Choose {
        round: u32,
        total_rounds: u32,
        history: String,
    },
}

// No terminal phase: in the outcome contract the session ends via
// `Transition::End` (which derives the outcome and emits `SessionEnd`), not by
// moving to a "finished" phase. The terminal signal is session-end + the
// derived `Outcome` receipt, so `Playing` is the last program phase.
#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Waiting for opponent")]
    Setup,
    #[phase(description = "Round in progress")]
    Playing,
}

/// Derived terminal receipt: a pure projection from final shared state.
///
/// Computed in absolute participant order (`scores[0]` is participant 0), so
/// every party derives the identical outcome from the agreed final state.
/// Higher cumulative payoff wins; equal totals draw.
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
    history: Vec<[Choice; 2]>,
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
    /// Score the completed round in ABSOLUTE participant order so every node runs
    /// the identical shared transition. `payoff(a, b).0` is `a`'s points, so
    /// `payoff(p0, p1)` yields `(p0_points, p1_points)` directly.
    fn score_round(&mut self) {
        let Some(vals) = self.commit_reveal.values() else {
            return;
        };
        let p0 = *vals[0];
        let p1 = *vals[1];
        let (pts0, pts1) = payoff(p0, p1);

        self.scores[0] += pts0;
        self.scores[1] += pts1;

        self.history.push([p0, p1]);
        self.round += 1;
    }

    fn is_game_over(&self) -> bool {
        self.round >= self.total_rounds
    }

    fn choice_request(&self, slot: usize) -> callouts::Choose {
        let mut history = String::new();
        for (i, choices) in self.history.iter().enumerate() {
            let mine = choices[slot];
            let theirs = choices[1 - slot];
            let (my_pts, their_pts) = payoff(mine, theirs);
            history.push_str(&format!(
                "R{}: you={mine} them={theirs} ({my_pts},{their_pts}). ",
                i + 1,
            ));
        }

        callouts::Choose {
            round: (self.round + 1) as u32,
            total_rounds: self.total_rounds as u32,
            history,
        }
    }
}

#[arena0::program(
    name = "prisoner-dilemma",
    display_name = "Prisoner's Dilemma",
    version = "1.0.0",
    description = "Prisoner's dilemma with commit-reveal",
    participants = 2,
    capabilities(auto)
)]
pub mod prisoner_dilemma {
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
        let current_round = if state.is_game_over() {
            state.total_rounds
        } else {
            state.round + 1
        };

        View::new()
            .header(vp.fit_text(format!(
                "Prisoner's dilemma - round {} of {}",
                current_round, state.total_rounds
            )))
            .agents(vp.fit_text(render_agents(state, None)))
            .state(vp.fit_text(render_state(state, None, vp)))
            .status_bar(vp.fit_text(format!(
                "{} - round {} of {}",
                phase_label(state.phase()),
                current_round,
                state.total_rounds
            )))
    }

    fn render_agents(state: &Shared, me: Option<usize>) -> String {
        let mut agents = String::new();
        for idx in player_order(me) {
            let _ = writeln!(
                agents,
                "{}: {} points",
                player_label(idx, me),
                state.scores[idx]
            );
        }
        agents
    }

    fn render_state(state: &Shared, me: Option<usize>, vp: &Viewport) -> String {
        let mut body = String::from(
            "Payoff matrix (row/column)\n\
                   C      D\n\
             C    3/3    0/5\n\
             D    5/0    1/1\n\
            \n\
             History\n",
        );
        if state.history.is_empty() {
            body.push_str("No rounds recorded");
        } else {
            for idx in player_order(me) {
                let _ = write!(body, "{}:", player_label(idx, me));
                for round in &state.history {
                    let _ = write!(body, " {}", render_choice(round[idx], vp));
                }
                body.push('\n');
            }
        }
        body
    }

    fn render_choice(choice: Choice, vp: &Viewport) -> String {
        if vp.color.supports_color() {
            format!("{}{}\x1b[0m", choice.sgr(), choice.glyph())
        } else {
            choice.glyph().to_string()
        }
    }

    fn player_order(me: Option<usize>) -> [usize; 2] {
        match me {
            Some(1) => [1, 0],
            _ => [0, 1],
        }
    }

    fn player_label(idx: usize, me: Option<usize>) -> String {
        // Role first, seat index second (L047): the human always reads
        // "you"/"opponent" first no matter which seat they hold.
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

    /// Position-0 boundary: set the match length. Shared handler, so it issues no
    /// callout and broadcasts nothing. The commit-reveal primitive starts from its
    /// `Default`.
    fn on_session_started(ctx: &mut SharedContext) -> Result<Transition<Phase>, ProgramFault> {
        ctx.mutate_shared(|s| s.total_rounds = 5);
        Ok(Transition::To(Phase::Playing))
    }

    /// Local decision hook: broadcast the owed reveal once every commit is in,
    /// otherwise ask the agent for this round's move when one is still owed.
    fn on_react(ctx: &mut Context) -> Result<(), ProgramFault> {
        // Unique-writer rule: react only when this node is the participant
        // whose action is next (first missing commit, then first missing
        // reveal); an idle node never broadcasts a sibling candidate.
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
            ctx.effects().callout(req).dispatch();
        }
        Ok(())
    }

    fn on_message(
        ctx: &mut SharedContext,
        from: Participant,
        msg: Message,
    ) -> Result<ApplyDecision<Phase>, ProtocolFault> {
        let Message::CommitReveal(cr_msg) = msg;
        // Unique-writer rule: only the expected writer may write here; any
        // other sender is a deterministic reject (no sibling candidates).
        if ctx.shared().commit_reveal.expected_writer() != Some(from) {
            return Ok(ApplyDecision::Reject);
        }
        if ctx.commit_reveal().handle(from, cr_msg).is_err() {
            return Ok(ApplyDecision::Reject);
        }
        if !ctx.shared().commit_reveal.is_complete() {
            return Ok(ApplyDecision::Accept(Transition::Stay));
        }

        let finished = ctx.mutate_shared(|s| {
            s.score_round();
            s.commit_reveal
                .reset()
                .expect("completed commit-reveal round can reset");
            s.is_game_over()
        });

        if finished {
            return Ok(ApplyDecision::Accept(Transition::End));
        }
        // The next round's callout is issued by `on_react` (needs_commit after reset).
        Ok(ApplyDecision::Accept(Transition::Stay))
    }

    fn on_input(ctx: &mut Context, input: Input) -> Result<(), InputFault> {
        let Input::Choose(choice) = input;
        ctx.commit_reveal().commit(choice)?.broadcast();
        Ok(())
    }

    fn on_query(_ctx: &SharedContext, _: ()) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0::testing::{ALICE, BOB, FaultStatus, Harness, Scenario, TestHarness};
    use arena0::types::{ColorDepth, Slot};
    use arena0_primitives::commit_reveal;

    fn local_b() -> PeerId {
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

    #[test]
    fn choice_strings_are_stable() {
        assert_eq!(Choice::Cooperate.to_string(), "cooperate");
        assert_eq!(Choice::Defect.to_string(), "defect");
    }

    #[test]
    fn payoff_matrix_is_canonical() {
        for (mine, theirs, expected) in [
            (Choice::Cooperate, Choice::Cooperate, (3, 3)),
            (Choice::Cooperate, Choice::Defect, (0, 5)),
            (Choice::Defect, Choice::Cooperate, (5, 0)),
            (Choice::Defect, Choice::Defect, (1, 1)),
        ] {
            assert_eq!(payoff(mine, theirs), expected);
        }
    }

    // -- Session setup --

    #[arena0::test(PrisonerDilemma, ())]
    fn session_start_establishes_the_game_contract(h: ()) {
        let fx = h.session_started(local_b());

        assert!(matches!(fx.fault, FaultStatus::None));
        assert_eq!(h.shared().total_rounds, 5);
        assert_eq!(h.shared().phase(), Phase::Playing);
        assert_eq!(h.shared().round, 0);
        assert!(fx.has_callout());
    }

    // -- Input handling --

    #[arena0::test(PrisonerDilemma, ())]
    fn choice_input_sends_commit(h: ()) {
        h.session_started(local_b());

        let fx = h.input(Input::Choose(Choice::Cooperate));
        assert!(matches!(fx.fault, FaultStatus::None));
        assert!(fx.has_broadcast());

        let msgs: Vec<Message> = fx.messages();
        assert!(matches!(
            msgs[0],
            Message::CommitReveal(commit_reveal::Message::Commit(_))
        ));
    }

    // -- Full game --

    use arena0_primitives::commit_reveal::CommitReveal;

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

    /// Play one full round on a single native replica (the local node is
    /// participant 0), hand-delivering every broadcast including self-delivery.
    /// `mine` is the local choice, `theirs` the opponent's.
    fn play_round(h: &mut TestHarness<PrisonerDilemma>, mine: Choice, theirs: Choice) {
        let fx = h.resolve_callout::<callouts::Choose>(mine);
        let my_commit = fx.messages::<Message>().remove(0);
        h.message(h.peer_id(), my_commit);
        let fx = h.message(local_b(), make_commit(theirs));
        let my_reveal = fx.messages::<Message>().remove(0);
        h.message(h.peer_id(), my_reveal);
        h.message(local_b(), make_reveal(theirs));
    }

    fn assert_scenario_scores(name: &str, rounds: &[(Choice, Choice)], expected_scores: [u32; 2]) {
        let mut scenario = Scenario::<PrisonerDilemma>::named(name);
        for &(alice, bob) in rounds {
            scenario = scenario
                .input(ALICE, Input::Choose(alice))
                .deliver_all()
                .input(BOB, Input::Choose(bob))
                .deliver_all();
        }
        let pair = scenario.run(());
        pair.trace().assert_shared_aligned();
        assert_eq!(pair.alice().shared().scores, expected_scores);
        assert_eq!(pair.bob().shared().scores, expected_scores);
        assert_eq!(
            pair.alice().shared().round,
            u8::try_from(rounds.len()).expect("test round count fits")
        );
        assert_eq!(pair.alice().shared().history.len(), rounds.len());
    }

    #[test]
    fn payoff_scenarios_converge_across_replicas() {
        for (name, rounds, scores) in [
            (
                "mutual cooperation",
                [(Choice::Cooperate, Choice::Cooperate); 5],
                [15, 15],
            ),
            (
                "mutual defection",
                [(Choice::Defect, Choice::Defect); 5],
                [5, 5],
            ),
            (
                "asymmetric choices",
                [(Choice::Defect, Choice::Cooperate); 5],
                [25, 0],
            ),
        ] {
            assert_scenario_scores(name, &rounds, scores);
        }
    }

    #[arena0::test(PrisonerDilemma, ())]
    fn history_records_correct_choices(ha: ()) {
        ha.session_started(local_b());

        // Local (participant 0) cooperates, opponent defects.
        play_round(&mut ha, Choice::Cooperate, Choice::Defect);

        assert_eq!(ha.shared().history.len(), 1);
        let round = &ha.shared().history[0];
        assert_eq!(round[0], Choice::Cooperate);
        assert_eq!(round[1], Choice::Defect);

        assert_eq!(ha.shared().scores, [0, 5]);
    }

    #[arena0::test(PrisonerDilemma, ())]
    fn view_renders_matrix_history_and_scores(ha: ()) {
        ha.session_started(local_b());
        play_round(&mut ha, Choice::Cooperate, Choice::Defect);

        let view = ha.view(Viewport {
            width: 80,
            color: ColorDepth::Ansi16,
        });
        let state = slot(&view, Slot::State);
        assert!(state.contains("Payoff matrix"));
        assert!(state.contains("3/3"));
        assert!(state.contains("0/5"));
        assert!(state.contains("History"));
        assert!(state.contains("C"));
        assert!(state.contains("D"));
        assert!(slot(&view, Slot::Agents).contains("P0"));
        assert!(slot(&view, Slot::Agents).contains("P1"));
        assert!(slot(&view, Slot::StatusBar).contains("round 2 of 5"));
    }

    #[arena0::test(PrisonerDilemma, ())]
    fn view_mono_contains_no_sgr(h: ()) {
        h.session_started(local_b());

        let view = h.view(Viewport {
            width: 80,
            color: ColorDepth::Mono,
        });
        assert_no_sgr(&view);
        assert!(slot(&view, Slot::State).contains("Payoff matrix"));
    }

    #[arena0::test(PrisonerDilemma, ())]
    fn game_not_finished_before_five_rounds(ha: ()) {
        ha.session_started(local_b());

        for _ in 0..4 {
            play_round(&mut ha, Choice::Cooperate, Choice::Cooperate);
        }

        assert_eq!(ha.shared().phase(), Phase::Playing);
        assert_eq!(ha.shared().round, 4);
    }

    // -- Outcome projection --

    #[test]
    fn outcome_projects_scores_and_winner() {
        for (scores, expected) in [
            (
                [25, 0],
                Outcome::Win {
                    winner: Participant::new(0),
                    scores: [25, 0],
                },
            ),
            (
                [0, 25],
                Outcome::Win {
                    winner: Participant::new(1),
                    scores: [0, 25],
                },
            ),
            ([15, 15], Outcome::Draw { scores: [15, 15] }),
        ] {
            let state = Shared {
                scores,
                ..Shared::default()
            };
            assert_eq!(PrisonerDilemma::outcome(&state), expected);
        }
    }

    #[test]
    fn input_schema_metadata() {
        let schemas = <Callout as Arena0Callout>::schemas();
        assert_eq!(schemas.len(), 1);

        let s = &schemas[0];
        assert_eq!(s.name, "Choose");
        assert!(!s.prompt.is_empty());
        assert_eq!(
            s.output.as_value()["enum"]
                .as_array()
                .expect("expected enum output schema")
                .len(),
            2
        );
        assert_eq!(
            s.input.as_value()["properties"]
                .as_object()
                .expect("expected object input schema")
                .len(),
            3
        );
    }
}
