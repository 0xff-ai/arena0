//! Test fixture for timer dispatch.
//!
//! `SessionStarted` arms a zero-delay `Timer::Fired`, whose handler counts
//! firings in the local state. `Timer::Forge` is the hostile-handler mode: the
//! handler forges a replacement context, and the generated dispatch glue must
//! reject the dispatch before the forged shared view can reach `callout`, and
//! must store neither image.

use std::time::Duration;

use arena0::prelude::*;

#[arena0::data]
pub enum Timer {
    /// Count one firing.
    Fired,
    /// Forge a replacement context.
    Forge,
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

#[arena0::callouts]
pub enum Callout {
    /// A callout that the forged shared image would open.
    #[arena0::callout(output = ())]
    Wait { marker: u8 },
}

#[arena0::program(
    name = "timer-dispatch",
    display_name = "Timer Dispatch Fixture",
    version = "1.0.0",
    description = "Test fixture for typed timer dispatch",
    participants = 2,
    capabilities(auto)
)]
pub mod timer_dispatch {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;
    type Callout = super::Callout;

    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
        _ensemble: &arena0::Ensemble,
    ) -> Result<ProgramTransition<TimerDispatch>, ProgramFault> {
        ctx.effects().set_timer(Timer::Fired, Duration::ZERO);
        Ok(Transition::Stay)
    }

    fn on_timer(ctx: &mut LocalContext<Shared, Local>, timer: Timer) -> Result<(), ProgramFault> {
        match timer {
            Timer::Fired => {
                ctx.mutate_local(|local| local.fired = local.fired.saturating_add(1));
            }
            Timer::Forge => {
                let forged_shared = Shared {
                    phase: arena0::ManagedPhase::__new(Phase::Waiting),
                    marker: 7,
                };
                let forged_local = Local { fired: 9 };
                // SAFETY: hostile-handler mode. A local handler must not be
                // able to replace its context; the generated glue rejects the
                // dispatch.
                *ctx = unsafe { LocalContext::__new(forged_shared, forged_local, ctx.identity()) };
            }
        }
        Ok(())
    }

    fn callout(ctx: &CalloutContext<Shared, Local>) -> Option<Callout> {
        (ctx.shared().marker != 0).then(|| Callout::Wait {
            marker: ctx.shared().marker,
        })
    }
}
