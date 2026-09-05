//! Minimal two-participant arena0 program.

use std::fmt::Write;

use arena0::prelude::*;

#[arena0::data]
#[derive(Copy, Default)]
pub enum Choice {
    #[default]
    One,
    Two,
}

impl Choice {
    const fn score(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
        }
    }
}

#[arena0::message]
pub enum Message {
    Choice(Choice),
}

#[arena0::callouts]
pub enum Callout {
    #[arena0::callout(output = Choice)]
    Choose { previous: Option<Choice> },
}

#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Waiting for both participants")]
    Setup,
    #[phase(description = "Collecting choices")]
    Choosing,
}

#[arena0::outcome]
pub enum Outcome {
    Win {
        winner: Participant,
        choices: [Choice; 2],
    },
    Draw {
        choices: [Choice; 2],
    },
}

#[arena0::state(max = 1024)]
pub struct Shared {
    #[phase]
    phase: Phase,
    choices: [Option<Choice>; 2],
}

#[arena0::local]
#[derive(Default)]
pub struct Local {}

#[arena0::program(
    name = "minimal-choice",
    display_name = "Minimal Choice",
    version = "1.0.0",
    description = "Two participants choose a small integer in public order",
    participants = 2,
    capabilities(auto)
)]
pub mod minimal_choice {
    use super::*;

    type Shared = super::Shared;
    type Local = super::Local;
    type Message = super::Message;
    type Callout = super::Callout;
    type Input = super::Input;
    type Outcome = super::Outcome;

    fn writer(state: &Shared) -> Option<Participant> {
        state
            .choices
            .iter()
            .position(Option::is_none)
            .map(|index| Participant::try_from(index).expect("two choices fit Participant"))
    }

    fn outcome(state: &Shared) -> Outcome {
        let choices = [
            state.choices[0].expect("terminal choice for P0"),
            state.choices[1].expect("terminal choice for P1"),
        ];
        match choices[0].score().cmp(&choices[1].score()) {
            std::cmp::Ordering::Greater => Outcome::Win {
                winner: Participant::new(0),
                choices,
            },
            std::cmp::Ordering::Less => Outcome::Win {
                winner: Participant::new(1),
                choices,
            },
            std::cmp::Ordering::Equal => Outcome::Draw { choices },
        }
    }

    fn view(ctx: &SharedContext, vp: &Viewport) -> View {
        let state = ctx.shared();
        let mut agents = String::new();
        for (index, choice) in state.choices.iter().enumerate() {
            let value = choice.map_or("waiting", |choice| match choice {
                Choice::One => "one",
                Choice::Two => "two",
            });
            let _ = writeln!(agents, "P{index}: {value}");
        }
        let received = state.choices.iter().flatten().count();
        let status = if received == 2 {
            "complete"
        } else {
            "choosing"
        };

        View::new()
            .header(vp.fit_text("Minimal choice"))
            .agents(vp.fit_text(agents))
            .state(vp.fit_text(format!("Choices received: {received} of 2")))
            .status_bar(vp.fit_text(status))
    }

    fn on_session_started(_ctx: &mut SharedContext) -> Result<Transition<Phase>, ProgramFault> {
        Ok(Transition::To(Phase::Choosing))
    }

    fn on_react(ctx: &mut Context) -> Result<(), ProgramFault> {
        if writer(ctx.shared()) == Some(ctx.me()) {
            let previous = ctx.shared().choices.iter().flatten().next().copied();
            ctx.effects()
                .callout(callouts::Choose { previous })
                .dispatch();
        }
        Ok(())
    }

    fn on_message(
        ctx: &mut SharedContext,
        from: Participant,
        message: Message,
    ) -> Result<ApplyDecision<Phase>, ProtocolFault> {
        if writer(ctx.shared()) != Some(from) {
            return Ok(ApplyDecision::Reject);
        }
        let Message::Choice(choice) = message;
        ctx.mutate_shared(|state| state.choices[from.index()] = Some(choice));
        if writer(ctx.shared()).is_none() {
            Ok(ApplyDecision::Accept(Transition::End))
        } else {
            Ok(ApplyDecision::Accept(Transition::Stay))
        }
    }

    fn on_input(ctx: &mut Context, input: Input) -> Result<(), InputFault> {
        if writer(ctx.shared()) != Some(ctx.me()) {
            return Err(anyhow!("this participant does not own the next choice").into());
        }
        let Input::Choose(choice) = input;
        ctx.effects().broadcast(&Message::Choice(choice));
        Ok(())
    }

    fn on_query(_ctx: &SharedContext, _: ()) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0::testing::{ALICE, BOB, Harness, Scenario};
    use arena0::types::{ColorDepth, Slot};

    #[test]
    fn two_replicas_converge_on_the_choices() {
        let pair = Scenario::<minimal_choice::MinimalChoice>::named("two public choices")
            .input(ALICE, Input::Choose(Choice::One))
            .deliver_all()
            .input(BOB, Input::Choose(Choice::Two))
            .deliver_all()
            .run(());

        pair.trace().assert_shared_aligned();
        assert_eq!(pair.alice().shared().choices[0], Some(Choice::One));
        assert_eq!(pair.alice().shared().choices[1], Some(Choice::Two));
    }

    #[arena0::test(MinimalChoice, ())]
    fn view_uses_all_four_slots_and_plain_text(h: ()) {
        h.session_started(PeerId([1; 32]));
        let view = h.view(Viewport {
            width: 80,
            color: ColorDepth::Mono,
        });

        for slot in [Slot::Header, Slot::Agents, Slot::State, Slot::StatusBar] {
            assert!(view.slots.contains_key(&slot));
        }
        assert!(view.slots.values().all(|text| !text.contains("\x1b[")));
    }
}
