/// Convenience re-exports for program authors. `use arena0::prelude::*;`
pub mod prelude {
    pub use crate::{
        AbortReason, ApplyDecision, Arena0Callout, Arena0CalloutRequest, Arena0Pending,
        Arena0Phase, Arena0Query, Arena0TypedCalloutRequest, ArenaFuture, CalloutBuilder,
        CalloutSpec, Capability, Committed, Context, DisconnectReason, DivergenceDiagnostic,
        DivergenceKind, Effect, Effects, Ensemble, EnsembleError, Event, HashAlgorithm, InputFault,
        IntoTimerEffect, LocalDebug, LocalPrimitiveField, LocalState, LogLevel, MessageApply, Open,
        Participant, ParticipantCount, PeerId, PendingDecl, PendingKind, PendingRecord,
        PhasedProgram, PhasedSharedState, PhaselessTransition, Primitive, PrimitiveOutput,
        PrimitiveOutputs, PrimitiveRoute, PrimitiveRouteSchema, Program, ProgramDefinition,
        ProgramFault, ProgramMetadata, ProgramQuery, ProgramSchema, ProgramValue, ProgramView,
        ProtocolFault, QuerySchema, RawPrimitiveRoute, Retryable, SharedContext,
        SharedPrimitiveField, SharedState, SignBuilder, SignScheme, StateHash, StateSchema,
        TimerPayload, TimerSchedule, TimerSpec, TraceEntry, Transition, View, Viewport, retryable,
        timer_payload,
    };
    pub use anyhow::{anyhow, bail, ensure};
    pub use borsh;
    pub use serde::{Deserialize, Serialize};
}
