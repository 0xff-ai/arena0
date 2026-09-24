use arena0::prelude::*;

#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Waiting for the timer")]
    Waiting,
}

#[arena0::state(max = 64)]
pub struct Shared {
    #[phase]
    phase: Phase,
    marker: u8,
}

#[arena0::local]
#[derive(Default)]
pub struct Local {
    fired: u8,
}

#[arena0::program(
    name = "timer-dispatch-unit",
    display_name = "Timer Dispatch Unit Fixture",
    version = "1.0.0",
    description = "Test fixture for unit timer dispatch",
    participants = 2,
    capabilities(auto)
)]
pub mod timer_dispatch_unit {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;

    fn on_react(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<ProgramTransition<TimerDispatchUnit>, ProgramFault> {
        ctx.effects().set_timer(0, ());
        Ok(Transition::Stay)
    }

    fn on_timer(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<ProgramTransition<TimerDispatchUnit>, ProgramFault> {
        ctx.mutate_local(|local| local.fired = local.fired.saturating_add(1));
        Ok(Transition::Stay)
    }
}
