//! Native coverage for the single-dispatch agreed-step fold.
//!
//! The generated dispatch glue decodes `Event::MessagesReceived` and calls
//! [`arena0::__apply_step`]. These tests are that glue: they build the agreed
//! context directly and drive the fold through its public entry point.

use arena0::prelude::*;

/// Payload that makes a handler return `Transition::To(Phase::B)`.
const ADVANCE: u8 = 200;
/// Payload that makes a handler return `Transition::End`.
const END: u8 = 201;
/// Payload that makes a handler deterministically reject the step.
const REJECT: u8 = 202;

#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "First phase")]
    A,
    #[phase(description = "Second phase")]
    B,
}

#[arena0::state(max = 256)]
pub struct Shared {
    #[phase]
    phase: Phase,
    applied: Vec<u8>,
}

#[arena0::message]
pub enum Message {
    /// Record `value` in `applied`, then act on the reserved payloads.
    Value(u8),
    /// Record the phase the handler observed.
    PhaseMark,
}

#[arena0::program(
    name = "step-fold",
    display_name = "Step Fold",
    version = "1.0.0",
    description = "Native fold fixture",
    participants = 3,
    capabilities(auto)
)]
pub mod step_fold {
    use super::*;

    type Shared = super::Shared;
    type Message = super::Message;

    fn on_message(
        ctx: &mut Context<Shared>,
        _from: Participant,
        msg: Message,
    ) -> MessageApply<StepFold> {
        match msg {
            Message::Value(REJECT) => Ok(ApplyDecision::Reject),
            Message::Value(ADVANCE) => {
                ctx.mutate_shared(|shared| shared.applied.push(ADVANCE));
                Ok(ApplyDecision::Accept(Transition::To(Phase::B)))
            }
            Message::Value(END) => {
                ctx.mutate_shared(|shared| shared.applied.push(END));
                Ok(ApplyDecision::Accept(Transition::End))
            }
            Message::Value(value) => {
                ctx.mutate_shared(|shared| shared.applied.push(value));
                Ok(ApplyDecision::Accept(Transition::Stay))
            }
            Message::PhaseMark => {
                let seen = match ctx.shared().phase() {
                    Phase::A => 0,
                    Phase::B => 1,
                };
                ctx.mutate_shared(|shared| shared.applied.push(seen));
                Ok(ApplyDecision::Accept(Transition::Stay))
            }
        }
    }
}

#[arena0::program(
    name = "reordered-fold",
    display_name = "Reordered Fold",
    version = "1.0.0",
    description = "Native canonicalize fixture",
    participants = 3,
    capabilities(auto)
)]
pub mod reordered_fold {
    use super::*;

    type Shared = super::Shared;
    type Message = super::Message;

    fn canonicalize(_shared: &Shared, messages: &mut [(Participant, Message)]) {
        let key = |msg: &Message| match msg {
            Message::Value(value) => *value,
            Message::PhaseMark => 0,
        };
        messages.sort_by(|left, right| key(&right.1).cmp(&key(&left.1)));
    }

    fn on_message(
        ctx: &mut Context<Shared>,
        _from: Participant,
        msg: Message,
    ) -> MessageApply<ReorderedFold> {
        if let Message::Value(value) = msg {
            ctx.mutate_shared(|shared| shared.applied.push(value));
        }
        Ok(ApplyDecision::Accept(Transition::Stay))
    }
}

/// Build the agreed context the way the generated dispatch glue does.
///
/// # Safety
///
/// `Ctx::__new` requires that only the dispatch of an agreed event builds an
/// agreed context. This test *is* that dispatch: it calls `__apply_step` as
/// the sole owner of the step and never emits effects through the handle.
fn agreed_ctx() -> Context<Shared> {
    unsafe { Context::__new(Shared::default(), (), PeerId([0u8; 32])) }
}

#[test]
fn default_order_is_the_hosts_order() {
    let mut ctx = agreed_ctx();
    let messages = vec![
        (Participant::new(2), Message::Value(30)),
        (Participant::new(0), Message::Value(10)),
        (Participant::new(1), Message::Value(20)),
    ];

    let decision = arena0::__apply_step::<StepFold>(&mut ctx, messages).unwrap();

    assert_eq!(decision, ApplyDecision::Accept(Transition::Stay));
    assert_eq!(ctx.shared().applied, vec![30, 10, 20]);
}

#[test]
fn canonicalize_reorders_before_any_handler_runs() {
    let mut ctx = agreed_ctx();
    let messages = vec![
        (Participant::new(2), Message::Value(10)),
        (Participant::new(0), Message::Value(30)),
        (Participant::new(1), Message::Value(20)),
    ];

    let decision = arena0::__apply_step::<ReorderedFold>(&mut ctx, messages).unwrap();

    assert_eq!(decision, ApplyDecision::Accept(Transition::Stay));
    assert_eq!(ctx.shared().applied, vec![30, 20, 10]);
}

#[test]
fn a_reject_rejects_the_whole_step() {
    let mut ctx = agreed_ctx();
    let messages = vec![
        (Participant::new(0), Message::Value(10)),
        (Participant::new(1), Message::Value(REJECT)),
    ];

    let decision = arena0::__apply_step::<StepFold>(&mut ctx, messages).unwrap();

    assert_eq!(decision, ApplyDecision::Reject);
    assert_eq!(ctx.shared().applied, vec![10]);
}

#[test]
fn a_phase_change_is_visible_to_the_next_message() {
    let mut ctx = agreed_ctx();
    let messages = vec![
        (Participant::new(0), Message::Value(ADVANCE)),
        (Participant::new(1), Message::PhaseMark),
    ];

    let decision = arena0::__apply_step::<StepFold>(&mut ctx, messages).unwrap();

    assert_eq!(decision, ApplyDecision::Accept(Transition::Stay));
    assert_eq!(ctx.shared().phase(), Phase::B);
    assert_eq!(ctx.shared().applied, vec![ADVANCE, 1]);
}

#[test]
fn end_stops_the_fold() {
    let mut ctx = agreed_ctx();
    let messages = vec![
        (Participant::new(0), Message::Value(END)),
        (Participant::new(1), Message::Value(20)),
    ];

    let decision = arena0::__apply_step::<StepFold>(&mut ctx, messages).unwrap();

    assert_eq!(decision, ApplyDecision::Accept(Transition::End));
    assert_eq!(ctx.shared().applied, vec![END]);
}
