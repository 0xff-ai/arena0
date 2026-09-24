use std::time::Duration;

use arena0::prelude::*;

#[arena0::data]
enum Timer {
    Fired,
}

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
    name = "timer-dispatch-typed",
    display_name = "Timer Dispatch Typed Fixture",
    version = "1.0.0",
    description = "Test fixture for typed timer dispatch",
    participants = 2,
    capabilities(auto)
)]
pub mod timer_dispatch_typed {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;

    fn on_react(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<ProgramTransition<TimerDispatchTyped>, ProgramFault> {
        ctx.effects()
            .set_timer(Timer::Fired, TimerSchedule::after(Duration::ZERO));
        Ok(Transition::Stay)
    }

    fn on_timer(
        ctx: &mut Context<Shared, Local>,
        timer: Timer,
    ) -> Result<ProgramTransition<TimerDispatchTyped>, ProgramFault> {
        match timer {
            Timer::Fired => {}
        }
        ctx.mutate_local(|local| local.fired = local.fired.saturating_add(1));
        Ok(Transition::Stay)
    }
}
