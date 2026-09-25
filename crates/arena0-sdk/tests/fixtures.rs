use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};

use arena0::testing::{Harness, TestHarness};
use arena0::{
    ApplyDecision, CalloutContext, Context, Ensemble, LocalContext, MessageApply, Participant,
    PeerId, Primitive, Program, ProgramFault, ProgramTransition, ProgramValue, SharedState,
    TimerPayload, Transition,
};

#[derive(
    Default,
    Debug,
    Clone,
    PartialEq,
    Eq,
    arena0::serde::Serialize,
    arena0::serde::Deserialize,
    arena0::borsh::BorshSerialize,
    arena0::borsh::BorshDeserialize,
    arena0::schemars::JsonSchema,
)]
struct Shared {
    sender: Option<Participant>,
}

impl Primitive for Shared {}
impl ProgramValue for Shared {}

impl SharedState for Shared {
    const STATE_MAX: usize = 64;
}

#[derive(Default, arena0::borsh::BorshSerialize, arena0::borsh::BorshDeserialize)]
struct Local {
    me: Option<Participant>,
    ensemble_len: u8,
}

struct EnsembleProgram;

impl Program for EnsembleProgram {
    type Shared = Shared;
    type Local = Local;
    type Phase = Infallible;
    type Message = ();
    type Callout = ();
    type Input = ();
    type Params = ();
    type Outcome = ();

    fn outcome(_shared: &Self::Shared) -> Self::Outcome {}

    fn writer(_shared: &Self::Shared) -> Option<Participant> {
        None
    }

    fn on_session_started(
        ctx: &mut Context<Self::Shared, Self::Local>,
        ensemble: &Ensemble,
    ) -> Result<ProgramTransition<Self>, ProgramFault> {
        let me = ctx.me();
        let ensemble_len = u8::try_from(ensemble.len()).expect("test ensemble fits in u8");
        ctx.mutate_local(|local| {
            local.me = Some(me);
            local.ensemble_len = ensemble_len;
        });
        Ok(Transition::Stay)
    }

    fn on_message(
        ctx: &mut Context<Self::Shared, Self::Local>,
        from: Participant,
        _msg: Self::Message,
    ) -> MessageApply<Self> {
        ctx.shared_mut().sender = Some(from);
        Ok(ApplyDecision::Accept(Transition::Stay))
    }
}

fn peer(index: u8) -> PeerId {
    PeerId([index; 32])
}

fn started_n_party_harness() -> TestHarness<EnsembleProgram> {
    let ensemble =
        Ensemble::from_peers(vec![peer(0), peer(1), peer(2), peer(3)]).expect("test ensemble");
    let mut harness = TestHarness::<EnsembleProgram>::with_peer_id(peer(2), ());
    harness.session_started_with_ensemble(ensemble);
    harness
}

#[test]
fn n_party_harness_uses_canonical_local_and_sender_indices() {
    let mut harness = started_n_party_harness();

    assert_eq!(harness.local().me, Some(Participant::new(2)));
    assert_eq!(harness.local().ensemble_len, 4);
    assert!(harness.peer().is_none());

    harness.message(peer(1), ());
    assert_eq!(harness.shared().sender, Some(Participant::new(1)));
}

#[test]
fn native_session_start_rejects_an_ensemble_without_the_local_peer() {
    let mut harness = TestHarness::<EnsembleProgram>::with_peer_id(peer(9), ());
    let ensemble = Ensemble::from_peers(vec![peer(0), peer(1), peer(2)]).expect("test ensemble");

    let result = catch_unwind(AssertUnwindSafe(|| {
        harness.session_started_with_ensemble(ensemble);
    }));

    assert!(result.is_err());
    assert!(harness.trace().is_empty());
    assert_eq!(harness.shared().sender, None);
    assert_eq!(harness.local().me, None);
}

#[test]
fn n_party_replay_uses_canonical_diagnostics_and_rejects_bad_membership() {
    let mut harness = started_n_party_harness();
    harness.message(peer(1), ());
    let local = harness.peer_id();
    let trace = harness.trace().to_vec();

    let report = TestHarness::<EnsembleProgram>::replay_trace(local, (), &trace)
        .expect("N-party trace replays");
    assert_eq!(report.event_count, 2);

    let mut start_mismatch = trace[..1].to_vec();
    start_mismatch[0].post_state = arena0::StateHash([9; 32]);
    let error = TestHarness::<EnsembleProgram>::replay_trace(local, (), &start_mismatch)
        .expect_err("changed session-start state must diverge");
    assert_eq!(error.kind, arena0::DivergenceKind::PostStateMismatch);
    assert_eq!(error.participant, None, "session start has no author");

    let mut post_state_mismatch = trace.clone();
    post_state_mismatch[1].post_state = arena0::StateHash([9; 32]);
    let error = TestHarness::<EnsembleProgram>::replay_trace(local, (), &post_state_mismatch)
        .expect_err("changed message state must diverge");
    assert_eq!(error.kind, arena0::DivergenceKind::PostStateMismatch);
    assert_eq!(error.participant, Some(Participant::new(1)));

    let missing_local = TestHarness::<EnsembleProgram>::replay_trace(peer(9), (), &trace)
        .expect_err("replay must reject an ensemble without the local peer");
    assert_eq!(missing_local.kind, arena0::DivergenceKind::EventMismatch);
    assert_eq!(missing_local.field_path, "event.ensemble");
    assert_eq!(missing_local.participant, None);
    assert!(missing_local.left.contains(&peer(9).to_string()));
    assert_eq!(
        missing_local.right,
        format!("{:?}", [peer(0), peer(1), peer(2), peer(3)])
    );

    let mut missing_sender = trace.clone();
    match &mut missing_sender[1].event {
        arena0::types::Event::MessageReceived { from, .. } => *from = peer(9),
        event => panic!("expected message event, got {event:?}"),
    }
    let missing_sender = TestHarness::<EnsembleProgram>::replay_trace(local, (), &missing_sender)
        .expect_err("replay must reject a sender outside the ensemble");
    assert_eq!(missing_sender.kind, arena0::DivergenceKind::EventMismatch);
    assert_eq!(missing_sender.field_path, "event.from");
    assert_eq!(missing_sender.participant, None);
}

#[arena0::data(bound = "T: ::arena0::ProgramValue")]
#[serde(bound = "T: ::arena0::serde::Serialize + ::arena0::serde::de::DeserializeOwned")]
struct ExplicitBounds<T> {
    value: T,
}

#[test]
fn public_data_macro_compiles_explicit_generic_contract_bounds() {
    let value = ExplicitBounds {
        value: String::from("compiled"),
    };
    let encoded = arena0::borsh::to_vec(&value).expect("Borsh encoding");
    let decoded: ExplicitBounds<String> =
        arena0::borsh::from_slice(&encoded).expect("Borsh decoding");
    assert_eq!(decoded.value, value.value);
    assert!(
        ExplicitBounds::<String>::json_schema()
            .as_value()
            .is_object()
    );
}

/// A hostile local handler that forges a replacement context. The native
/// harness must reject the dispatch before the forged shared view reaches the
/// callout, and must restore both images.
struct ReplacingLocalProgram;

impl Program for ReplacingLocalProgram {
    type Shared = Shared;
    type Local = Local;
    type Phase = Infallible;
    type Message = ();
    type Callout = ();
    type Input = ();
    type Params = ();
    type Outcome = ();

    fn outcome(_shared: &Self::Shared) -> Self::Outcome {}

    fn writer(_shared: &Self::Shared) -> Option<Participant> {
        None
    }

    fn on_session_started(
        ctx: &mut Context<Self::Shared, Self::Local>,
        ensemble: &Ensemble,
    ) -> Result<ProgramTransition<Self>, ProgramFault> {
        let me = ctx.me();
        let ensemble_len = u8::try_from(ensemble.len()).expect("test ensemble fits in u8");
        ctx.mutate_local(|local| {
            local.me = Some(me);
            local.ensemble_len = ensemble_len;
        });
        Ok(Transition::Stay)
    }

    fn on_timer(
        ctx: &mut LocalContext<Self::Shared, Self::Local>,
        _timer: TimerPayload,
    ) -> Result<(), ProgramFault> {
        let forged_shared = Shared {
            sender: Some(Participant::new(3)),
        };
        let forged_local = Local {
            me: Some(Participant::new(3)),
            ensemble_len: 99,
        };
        // SAFETY: hostile-handler test. A local handler must not be able to
        // replace its context; the harness rejects the dispatch.
        *ctx = unsafe { LocalContext::__new(forged_shared, forged_local, ctx.identity()) };
        Ok(())
    }

    fn callout(ctx: &CalloutContext<'_, Self::Shared, Self::Local>) -> Option<Self::Callout> {
        ctx.shared().sender.is_none().then_some(())
    }
}

#[test]
fn native_local_handler_cannot_forge_a_shared_view_for_the_callout() {
    let mut harness = TestHarness::<ReplacingLocalProgram>::with_peer_id(peer(2), ());
    harness.session_started(peer(1));
    let pending_before = harness.active_pending().map(|callout| callout.id);
    assert!(pending_before.is_some(), "session start opens a callout");
    let sender_before = harness.shared().sender;
    let me_before = harness.local().me;
    let len_before = harness.local().ensemble_len;

    let result = harness.timer();

    assert!(result.rejected, "the forged shared view must be rejected");
    assert_eq!(harness.shared().sender, sender_before);
    assert_eq!(harness.local().me, me_before);
    assert_eq!(harness.local().ensemble_len, len_before);
    assert_eq!(
        harness.active_pending().map(|callout| callout.id),
        pending_before,
        "the open callout is unchanged"
    );
}

/// A shared DTO whose custom Borsh serializer fails for the value a local
/// handler installs. The native harness must treat that as a changed image and
/// return a plain rejection instead of panicking.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    arena0::serde::Serialize,
    arena0::serde::Deserialize,
    arena0::borsh::BorshDeserialize,
    arena0::schemars::JsonSchema,
)]
#[schemars(crate = "arena0::schemars")]
struct FallibleShared {
    poison: bool,
}

impl Default for FallibleShared {
    fn default() -> Self {
        Self { poison: false }
    }
}

impl arena0::borsh::BorshSerialize for FallibleShared {
    fn serialize<W: arena0::borsh::io::Write>(
        &self,
        writer: &mut W,
    ) -> arena0::borsh::io::Result<()> {
        if self.poison {
            return Err(arena0::borsh::io::Error::new(
                arena0::borsh::io::ErrorKind::Other,
                "poisoned shared image",
            ));
        }
        arena0::borsh::BorshSerialize::serialize(&self.poison, writer)
    }
}

impl Primitive for FallibleShared {}
impl ProgramValue for FallibleShared {}

impl SharedState for FallibleShared {
    const STATE_MAX: usize = 64;
}

struct PoisoningLocalProgram;

impl Program for PoisoningLocalProgram {
    type Shared = FallibleShared;
    type Local = Local;
    type Phase = Infallible;
    type Message = ();
    type Callout = ();
    type Input = ();
    type Params = ();
    type Outcome = ();

    fn outcome(_shared: &Self::Shared) -> Self::Outcome {}

    fn writer(_shared: &Self::Shared) -> Option<Participant> {
        None
    }

    fn on_session_started(
        ctx: &mut Context<Self::Shared, Self::Local>,
        ensemble: &Ensemble,
    ) -> Result<ProgramTransition<Self>, ProgramFault> {
        let me = ctx.me();
        let ensemble_len = u8::try_from(ensemble.len()).expect("test ensemble fits in u8");
        ctx.mutate_local(|local| {
            local.me = Some(me);
            local.ensemble_len = ensemble_len;
        });
        Ok(Transition::Stay)
    }

    fn on_timer(
        ctx: &mut LocalContext<Self::Shared, Self::Local>,
        _timer: TimerPayload,
    ) -> Result<(), ProgramFault> {
        // Emit provisional effects and logs that a rejected dispatch discards.
        ctx.log("before forging");
        ctx.effects().set_timer(0, ());
        let poison = FallibleShared { poison: true };
        let forged_local = Local {
            me: Some(Participant::new(3)),
            ensemble_len: 99,
        };
        // SAFETY: hostile-handler test. The harness must reject the dispatch
        // because the installed image cannot serialize.
        *ctx = unsafe { LocalContext::__new(poison, forged_local, ctx.identity()) };
        Ok(())
    }

    fn callout(ctx: &CalloutContext<'_, Self::Shared, Self::Local>) -> Option<Self::Callout> {
        (!ctx.shared().poison).then_some(())
    }
}

#[test]
fn native_local_handler_that_breaks_shared_serialization_is_rejected_without_panicking() {
    let mut harness = TestHarness::<PoisoningLocalProgram>::with_peer_id(peer(2), ());
    harness.session_started(peer(1));
    let pending_before = harness.active_pending().map(|callout| callout.id);
    assert!(pending_before.is_some(), "session start opens a callout");
    let me_before = harness.local().me;
    let len_before = harness.local().ensemble_len;

    let result = harness.timer();

    assert!(
        result.rejected,
        "an unserializable shared image must be rejected"
    );
    assert!(
        result.effects.is_empty(),
        "no effects survive a rejected dispatch"
    );
    assert!(
        result.logs.is_empty(),
        "no logs survive a rejected dispatch"
    );
    assert!(
        !harness.shared().poison,
        "the committed shared image is unchanged"
    );
    assert_eq!(harness.local().me, me_before);
    assert_eq!(harness.local().ensemble_len, len_before);
    assert_eq!(
        harness.active_pending().map(|callout| callout.id),
        pending_before,
        "the open callout is unchanged"
    );
}
