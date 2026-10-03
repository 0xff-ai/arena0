//! Two simultaneous verified transfers in opposite directions. Each
//! participant imports its file into its Host's blob store, grants it to the
//! execution, and the params name both objects and who sends the input. Direct
//! progress is local; receivers send their transfer's agreed message in transfer
//! order.

use arena0::prelude::*;
use arena0_primitives::verified_transfer::{
    self as transfer, DirectMessage, TransferStatus, TransferTimer, VerifiedTransfer as Transfer,
    VerifiedTransferLocal,
};

#[arena0::data]
pub struct Params {
    /// The participant that sends `input`; the other sends `result`.
    pub input_sender: PeerId,
    pub input_hash: BlobHash,
    pub input_length: u64,
    pub result_hash: BlobHash,
    pub result_length: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use verified_transfer::VerifiedTransfer;

    #[test]
    fn transfers_assign_roles_from_input_sender() {
        let ensemble = Ensemble::from_peers(vec![PeerId([1; 32]), PeerId([0; 32])]).unwrap();
        let mut params = Params {
            input_sender: PeerId([1; 32]),
            input_hash: BlobHash([2; 32]),
            input_length: 1,
            result_hash: BlobHash([3; 32]),
            result_length: 1,
        };
        let (input, result) = transfers(&params, &ensemble).unwrap();
        assert_eq!(input.sender(), Participant::new(1));
        assert_eq!(input.receiver(), Participant::new(0));
        assert_eq!(result.sender(), Participant::new(0));
        assert_eq!(result.receiver(), Participant::new(1));
        params.input_sender = PeerId([9; 32]);
        assert!(transfers(&params, &ensemble).is_err());
    }

    #[test]
    fn next_sender_is_the_receiver_of_the_first_unsettled_transfer() {
        let shared = Shared {
            input: Transfer::new(
                0,
                Participant::new(1),
                Participant::new(0),
                BlobHash([2; 32]),
                1,
            )
            .unwrap(),
            result: Transfer::new(
                1,
                Participant::new(0),
                Participant::new(1),
                BlobHash([3; 32]),
                1,
            )
            .unwrap(),
            ..Shared::default()
        };
        // SAFETY: only pure agreed-message handlers run; no host effects are applied.
        let mut ctx = unsafe { Context::__new(shared, Local::default(), PeerId([0; 32])) };
        assert_eq!(next_sender(ctx.shared()), Some(Participant::new(0)));
        let before = borsh::to_vec(ctx.shared()).unwrap();
        assert!(matches!(
            VerifiedTransfer::on_message(
                &mut ctx,
                Participant::new(1),
                Message::Result(transfer::TransferMessage::Complete)
            )
            .unwrap(),
            ApplyDecision::Reject
        ));
        assert_eq!(borsh::to_vec(ctx.shared()).unwrap(), before);
        assert!(ctx.shared().input.status().is_none());
        assert!(ctx.shared().result.status().is_none());
        assert!(matches!(
            VerifiedTransfer::on_message(
                &mut ctx,
                Participant::new(1),
                Message::Input(transfer::TransferMessage::Complete)
            )
            .unwrap(),
            ApplyDecision::Reject
        ));
        assert_eq!(borsh::to_vec(ctx.shared()).unwrap(), before);
        assert!(ctx.shared().input.status().is_none());
        assert_eq!(next_sender(ctx.shared()), Some(Participant::new(0)));
        assert!(matches!(
            VerifiedTransfer::on_message(
                &mut ctx,
                Participant::new(0),
                Message::Input(transfer::TransferMessage::Complete)
            )
            .unwrap(),
            ApplyDecision::Accept(Transition::Stay)
        ));
        assert_eq!(next_sender(ctx.shared()), Some(Participant::new(1)));
        let before = borsh::to_vec(ctx.shared()).unwrap();
        assert!(matches!(
            VerifiedTransfer::on_message(
                &mut ctx,
                Participant::new(0),
                Message::Result(transfer::TransferMessage::Failed)
            )
            .unwrap(),
            ApplyDecision::Reject
        ));
        assert_eq!(borsh::to_vec(ctx.shared()).unwrap(), before);
        assert!(ctx.shared().result.status().is_none());
        assert!(matches!(
            VerifiedTransfer::on_message(
                &mut ctx,
                Participant::new(1),
                Message::Result(transfer::TransferMessage::Failed)
            )
            .unwrap(),
            ApplyDecision::Accept(Transition::End)
        ));
        assert_eq!(next_sender(ctx.shared()), None);
    }

    #[test]
    fn outcome_reports_both_statuses() {
        let shared = Shared {
            input: Transfer::new(
                0,
                Participant::new(0),
                Participant::new(1),
                BlobHash([2; 32]),
                1,
            )
            .unwrap(),
            result: Transfer::new(
                1,
                Participant::new(1),
                Participant::new(0),
                BlobHash([3; 32]),
                1,
            )
            .unwrap(),
            ..Shared::default()
        };
        // SAFETY: the context models agreed dispatches without applying host effects.
        let mut ctx = unsafe { Context::__new(shared, Local::default(), PeerId([0; 32])) };
        assert!(matches!(
            VerifiedTransfer::on_message(
                &mut ctx,
                Participant::new(1),
                Message::Input(transfer::TransferMessage::Complete)
            )
            .unwrap(),
            ApplyDecision::Accept(Transition::Stay)
        ));
        assert!(matches!(
            VerifiedTransfer::on_message(
                &mut ctx,
                Participant::new(0),
                Message::Result(transfer::TransferMessage::Failed)
            )
            .unwrap(),
            ApplyDecision::Accept(Transition::End)
        ));
        let outcome = VerifiedTransfer::outcome(ctx.shared());
        assert_eq!(outcome.input, TransferStatus::Complete);
        assert_eq!(outcome.result, TransferStatus::Failed);
    }
}

/// The two transfers of `params` in `ensemble`: `input` from `input_sender`
/// to the other participant, `result` back.
pub fn transfers(
    params: &Params,
    ensemble: &Ensemble<Committed>,
) -> Result<(Transfer, Transfer), ProgramFault> {
    let sender = ensemble
        .participant_of(&params.input_sender)
        .ok_or_else(|| anyhow!("input_sender is not a participant"))?;
    let other = Participant::new(1 - sender.as_u8());
    let input = Transfer::new(0, sender, other, params.input_hash, params.input_length)?;
    let result = Transfer::new(1, other, sender, params.result_hash, params.result_length)?;
    Ok((input, result))
}

#[arena0::message]
pub enum Message {
    Input(transfer::TransferMessage),
    Result(transfer::TransferMessage),
}

#[arena0::outcome]
pub struct Outcome {
    pub input: TransferStatus,
    pub result: TransferStatus,
}

#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Transferring")]
    Transferring,
}

#[arena0::state(max = 4096)]
pub struct Shared {
    #[phase]
    phase: Phase,
    /// Held until session start resolves participant roles, then discarded;
    /// the two transfers own the resulting terms.
    params: Option<Params>,
    #[primitive(route = Message::Input)]
    input: transfer::VerifiedTransfer,
    #[primitive(route = Message::Result)]
    result: transfer::VerifiedTransfer,
}

#[arena0::local]
#[derive(Default)]
pub struct Local {
    input: VerifiedTransferLocal,
    result: VerifiedTransferLocal,
    /// A finished agreed message held while another transfer is ahead. Set only
    /// by `on_direct`, cleared by `on_message` when this participant becomes
    /// the next sender. At most one: each participant receives one transfer.
    pub held: Option<Message>,
}

/// The receiver of the first unsettled transfer: only receivers author
/// agreed messages.
fn next_sender(shared: &Shared) -> Option<Participant> {
    [&shared.input, &shared.result]
        .into_iter()
        .find(|transfer| transfer.status().is_none())
        .map(Transfer::receiver)
}

#[arena0::program(
    name = "verified-transfer",
    display_name = "Verified Transfer",
    version = "1.0.0",
    description = "Two verified file transfers in opposite directions",
    participants = 2,
    capabilities(auto)
)]
pub mod verified_transfer {
    use super::*;

    type Shared = super::Shared;
    type Local = super::Local;
    type Params = super::Params;
    type Message = super::Message;
    type Outcome = super::Outcome;

    fn initialize(shared: &mut Shared, params: Params) -> Result<(), ProgramFault> {
        shared.params = Some(params);
        Ok(())
    }

    /// Participant indexes follow the committed ensemble's sorted peers, so
    /// the roles are fixed here from `input_sender`, not at initialization.
    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<arena0::ProgramTransition<VerifiedTransfer>, ProgramFault> {
        let params = ctx.shared().params.as_ref().expect("initialized params");
        let (input, result) = transfers(params, ctx.ensemble())?;
        input.start(ctx);
        result.start(ctx);
        ctx.mutate_shared(|shared| {
            shared.params = None;
            shared.input = input;
            shared.result = result;
        });
        Ok(Transition::Stay)
    }

    fn on_timer(
        ctx: &mut LocalContext<Shared, Local>,
        timer: TransferTimer,
    ) -> Result<(), ProgramFault> {
        let TransferTimer::Send { transfer_id } = &timer;
        match transfer_id {
            0 => {
                let transfer = ctx.shared().input.clone();
                transfer.on_timer(ctx, |local| &mut local.input, timer);
            }
            1 => {
                let transfer = ctx.shared().result.clone();
                transfer.on_timer(ctx, |local| &mut local.result, timer);
            }
            _ => {}
        }
        Ok(())
    }

    fn on_direct(
        ctx: &mut LocalContext<Shared, Local>,
        from: Participant,
        msg: DirectMessage,
        attachment: Option<Attachment>,
    ) -> Result<(), ProgramFault> {
        let id = match &msg {
            DirectMessage::Leaves { transfer_id, .. }
            | DirectMessage::Chunk { transfer_id }
            | DirectMessage::Next { transfer_id }
            | DirectMessage::Missing { transfer_id } => *transfer_id,
        };
        match id {
            0 => {
                let transfer = ctx.shared().input.clone();
                if let Some(message) =
                    transfer.on_direct(ctx, |local| &mut local.input, from, msg, attachment)
                {
                    if next_sender(ctx.shared()) == Some(ctx.me()) {
                        broadcast(ctx, message, Message::Input)
                            .expect("one agreed message per transfer fits the queue");
                    } else {
                        ctx.local_mut().held = Some(Message::Input(message));
                    }
                }
            }
            1 => {
                let transfer = ctx.shared().result.clone();
                if let Some(message) =
                    transfer.on_direct(ctx, |local| &mut local.result, from, msg, attachment)
                {
                    if next_sender(ctx.shared()) == Some(ctx.me()) {
                        broadcast(ctx, message, Message::Result)
                            .expect("one agreed message per transfer fits the queue");
                    } else {
                        ctx.local_mut().held = Some(Message::Result(message));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Each transfer offers at most one agreed message and the queue holds
    /// 16, so a full queue is a programming error.
    fn broadcast<M: arena0::EffectMode>(
        ctx: &mut arena0::Ctx<Shared, Local, M>,
        message: transfer::TransferMessage,
        route: fn(transfer::TransferMessage) -> Message,
    ) -> M::Broadcast {
        ctx.primitive_output(message)
            .broadcast_via(&mut ctx.effects(), route)
    }

    fn on_message(
        ctx: &mut Context<Shared, Local>,
        from: Participant,
        msg: Message,
    ) -> arena0::MessageApply<VerifiedTransfer> {
        if next_sender(ctx.shared()) != Some(from) {
            return Ok(ApplyDecision::Reject);
        }
        match msg {
            Message::Input(message) => {
                Transfer::handle(ctx, |shared| &mut shared.input, from, message)?
            }
            Message::Result(message) => {
                Transfer::handle(ctx, |shared| &mut shared.result, from, message)?
            }
        }
        if ctx.local().held.is_some()
            && next_sender(ctx.shared()) == Some(ctx.me())
            && let Some(message) = ctx.local_mut().held.take()
        {
            match message {
                Message::Input(message) => broadcast(ctx, message, Message::Input),
                Message::Result(message) => broadcast(ctx, message, Message::Result),
            }
        }
        Ok(ApplyDecision::Accept(
            if next_sender(ctx.shared()).is_none() {
                Transition::End
            } else {
                Transition::Stay
            },
        ))
    }

    fn outcome(shared: &Shared) -> Outcome {
        Outcome {
            input: shared.input.status().unwrap_or(TransferStatus::Failed),
            result: shared.result.status().unwrap_or(TransferStatus::Failed),
        }
    }
}
