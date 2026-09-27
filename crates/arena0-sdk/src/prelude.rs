/// Convenience re-exports for program authors. `use arena0::prelude::*;`
pub mod prelude {
    pub use crate::{
        AbortReason, ApplyDecision, Arena0Callout, Arena0CalloutRequest, Arena0Phase, Arena0Query,
        Attachment, BlobError, BlobHandle, BlobHash, Blobs, CalloutContext, Capability, Committed,
        Context, Effect, Effects, Ensemble, EnsembleError, Event, LocalContext, LocalDebug,
        LocalState, LogLevel, MessageApply, Open, Participant, ParticipantCount, PeerId,
        PhasedSharedState, Primitive, PrimitiveField, PrimitiveOutput, PrimitiveRoute,
        PrimitiveRouteSchema, Program, ProgramDefinition, ProgramFault, ProgramMetadata,
        ProgramQuery, ProgramSchema, ProgramValue, ProgramView, ProtocolFault, QuerySchema,
        RangeAttachment, RawPrimitiveRoute, SendError, SharedState, SignScheme, Signed, StateHash,
        StateSchema, TimerPayload, Transition, VerifyError, View, Viewport, timer_payload,
    };
    pub use anyhow::{anyhow, bail, ensure};
    pub use borsh;
    pub use serde::{Deserialize, Serialize};
}
