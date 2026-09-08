use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};

use arena0::testing::{Harness, TestHarness};
use arena0::{
    ApplyDecision, Context, Ensemble, MessageApply, Participant, PeerId, Primitive, Program,
    ProgramFault, ProgramTransition, ProgramValue, SharedContext, SharedState, Transition,
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
        _ctx: &mut SharedContext<Self::Shared>,
        _ensemble: &Ensemble,
    ) -> Result<ProgramTransition<Self>, ProgramFault> {
        Ok(Transition::Stay)
    }

    fn on_react(ctx: &mut Context<Self::Shared, Self::Local>) -> Result<(), ProgramFault> {
        let me = ctx.me();
        let ensemble_len = u8::try_from(ctx.ensemble().len()).expect("test ensemble fits in u8");
        ctx.mutate_local(|local| {
            local.me = Some(me);
            local.ensemble_len = ensemble_len;
        });
        Ok(())
    }

    fn on_message(
        ctx: &mut SharedContext<Self::Shared>,
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
    assert_eq!(report.steps, 2);

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
        arena0::types::PublicEvent::MessageReceived { from, .. } => *from = peer(9),
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
