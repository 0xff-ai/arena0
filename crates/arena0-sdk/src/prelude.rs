/// Convenience re-exports for program authors. `use arena0::prelude::*;`
pub mod prelude {
    pub use crate::{
        AbortReason, ApplyDecision, Arena0Callout, Arena0CalloutRequest, Arena0Phase, Arena0Query,
        Arena0TypedCalloutRequest, BroadcastError, CalloutContext, CalloutSpec, Capability,
        Committed, Context, DisconnectReason, Effect, Effects, Ensemble, EnsembleError, Event,
        HashAlgorithm, IntoTimerEffect, LocalContext, LocalDebug, LocalState, LogLevel,
        MessageApply, Open, Participant, ParticipantCount, PeerId, PhasedProgram,
        PhasedSharedState, PhaselessTransition, Primitive, PrimitiveField, PrimitiveOutput,
        PrimitiveOutputs, PrimitiveRoute, PrimitiveRouteSchema, Program, ProgramDefinition,
        ProgramFault, ProgramMetadata, ProgramQuery, ProgramSchema, ProgramValue, ProgramView,
        ProtocolFault, QuerySchema, RawPrimitiveRoute, SharedState, SignScheme, Signed, StateHash,
        StateSchema, TimerPayload, TimerSchedule, TimerSpec, TraceEntry, Transition, View,
        Viewport, timer_payload,
    };
    pub use anyhow::{anyhow, bail, ensure};
    pub use borsh;
    pub use serde::{Deserialize, Serialize};
}
