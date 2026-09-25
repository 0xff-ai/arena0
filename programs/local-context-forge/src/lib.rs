//! Test fixture: a local handler forges a replacement context.
//!
//! The generated dispatch glue must reject the dispatch before the forged
//! shared view can reach `callout`, and must store neither image.

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
    edits: u8,
}

#[arena0::callouts]
pub enum Callout {
    /// A callout that the forged shared image would open.
    #[arena0::callout(output = ())]
    Wait { marker: u8 },
}

#[arena0::program(
    name = "local-context-forge",
    display_name = "Local Context Forge Fixture",
    version = "1.0.0",
    description = "Test fixture for a local handler forging its context",
    participants = 2,
    capabilities(auto)
)]
pub mod local_context_forge {
    use super::*;
    use arena0::ProgramTransition;

    type Shared = super::Shared;
    type Local = super::Local;
    type Callout = super::Callout;

    fn on_session_started(
        _ctx: &mut Context<Shared, Local>,
        _ensemble: &arena0::Ensemble,
    ) -> Result<ProgramTransition<LocalContextForge>, ProgramFault> {
        Ok(Transition::Stay)
    }

    fn on_timer(ctx: &mut LocalContext<Shared, Local>) -> Result<(), ProgramFault> {
        let forged_shared = Shared {
            phase: arena0::ManagedPhase::__new(Phase::Waiting),
            marker: 7,
        };
        let forged_local = Local { edits: 9 };
        // SAFETY: hostile-handler fixture. A local handler must not be able to
        // replace its context; the generated glue rejects the dispatch.
        *ctx = unsafe { LocalContext::__new(forged_shared, forged_local, ctx.identity()) };
        Ok(())
    }

    fn callout(ctx: &CalloutContext<Shared, Local>) -> Option<Callout> {
        (ctx.shared().marker != 0).then(|| Callout::Wait {
            marker: ctx.shared().marker,
        })
    }
}
