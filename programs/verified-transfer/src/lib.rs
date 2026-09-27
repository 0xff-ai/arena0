//! Two simultaneous verified transfers in opposite directions. Each
//! participant imports its file into its Host's blob store, grants it to the
//! execution, and the params name both objects and who sends the input. Direct
//! progress is local; each receiver authors its transfer's agreed result under
//! one writer at a time.

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
    // U5: demo tests.
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
    /// Set by initialization; the transfers are built from it at session start.
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

    /// The receiver of the first unsettled transfer: only receivers author
    /// agreed messages.
    fn writer(shared: &Shared) -> Option<Participant> {
        [&shared.input, &shared.result]
            .into_iter()
            .find(|transfer| transfer.status().is_none())
            .map(Transfer::receiver)
    }

    /// Participant indexes follow the committed ensemble's sorted peers, so
    /// the roles are fixed here from `input_sender`, not at initialization.
    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<arena0::ProgramTransition<VerifiedTransfer>, ProgramFault> {
        let params = ctx.shared().params.clone().expect("initialized params");
        let (input, result) = transfers(&params, ctx.ensemble())?;
        input.start(ctx);
        result.start(ctx);
        ctx.mutate_shared(|shared| {
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
                if let Some(message) = transfer.on_timer(ctx, |local| &mut local.input, timer) {
                    broadcast(ctx, message, Message::Input);
                }
            }
            1 => {
                let transfer = ctx.shared().result.clone();
                if let Some(message) = transfer.on_timer(ctx, |local| &mut local.result, timer) {
                    broadcast(ctx, message, Message::Result);
                }
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
                    broadcast(ctx, message, Message::Input);
                }
            }
            1 => {
                let transfer = ctx.shared().result.clone();
                if let Some(message) =
                    transfer.on_direct(ctx, |local| &mut local.result, from, msg, attachment)
                {
                    broadcast(ctx, message, Message::Result);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Each transfer offers at most one agreed message and the queue holds
    /// 16, so a full queue is a programming error.
    fn broadcast(
        ctx: &mut LocalContext<Shared, Local>,
        message: transfer::TransferMessage,
        route: fn(transfer::TransferMessage) -> Message,
    ) {
        ctx.primitive_output(message)
            .broadcast_via(&mut ctx.effects(), route)
            .expect("one agreed message per transfer fits the queue");
    }

    fn on_message(
        ctx: &mut Context<Shared, Local>,
        from: Participant,
        msg: Message,
    ) -> arena0::MessageApply<VerifiedTransfer> {
        match msg {
            Message::Input(message) => {
                Transfer::handle(ctx, |shared| &mut shared.input, from, message)?
            }
            Message::Result(message) => {
                Transfer::handle(ctx, |shared| &mut shared.result, from, message)?
            }
        }
        Ok(ApplyDecision::Accept(if writer(ctx.shared()).is_none() {
            Transition::End
        } else {
            Transition::Stay
        }))
    }

    fn outcome(shared: &Shared) -> Outcome {
        Outcome {
            input: shared.input.status().unwrap_or(TransferStatus::Failed),
            result: shared.result.status().unwrap_or(TransferStatus::Failed),
        }
    }
}
