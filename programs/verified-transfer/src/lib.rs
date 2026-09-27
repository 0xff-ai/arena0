//! Two simultaneous handle-only transfers. Each participant imports its source
//! into the Host blob store before starting the session and supplies its hash
//! and length as parameters. Direct progress is local; sender checkpoints are
//! agreed under one writer at a time.

use arena0::prelude::*;
use arena0_primitives::verified_transfer::{
    self as transfer, DirectMessage, TransferTimer, VerifiedTransfer as Transfer,
    VerifiedTransferLocal,
};

#[arena0::data]
pub struct Params {
    pub input_hash: BlobHash,
    pub input_length: u64,
    pub result_hash: BlobHash,
    pub result_length: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    type Demo = verified_transfer::VerifiedTransfer;

    fn initialized() -> Shared {
        let mut shared = Shared::default();
        <Demo as Program>::initialize(
            &mut shared,
            Params {
                input_hash: BlobHash([1; 32]),
                input_length: 1_000_000,
                result_hash: BlobHash([2; 32]),
                result_length: 1_000_001,
            },
        )
        .unwrap();
        shared
    }

    #[test]
    fn opposite_transfers_route_failures_and_advance_the_writer() {
        let shared = initialized();
        assert_eq!(shared.input.id(), 0);
        assert_eq!(shared.result.id(), 1);
        assert_eq!(shared.input.sender(), shared.result.receiver());
        assert_eq!(shared.input.receiver(), shared.result.sender());
        assert_eq!(shared.input.chunk_count(), 20);
        assert_eq!(shared.result.chunk_count(), 21);
        assert_eq!(shared.result.chunk_range(20).unwrap(), 1_000_000..1_000_001);
        assert_eq!(
            <Demo as Program>::writer(&shared),
            Some(Participant::new(0))
        );
        // SAFETY: this pure agreed-handler test owns the complete shared and
        // local images and invokes no Host effects or state-memory imports.
        let mut ctx = unsafe { Context::__new(shared, Local::default(), PeerId([1; 32])) };
        assert!(
            <Demo as Program>::on_message(
                &mut ctx,
                Participant::new(1),
                Message::Input(transfer::TransferMessage::Failed)
            )
            .is_err()
        );
        assert!(!ctx.shared().input.is_failed());
        assert!(matches!(
            <Demo as Program>::on_message(
                &mut ctx,
                Participant::new(0),
                Message::Input(transfer::TransferMessage::Failed)
            )
            .unwrap(),
            ApplyDecision::Accept(Transition::Stay)
        ));
        assert!(!ctx.shared().result.is_failed());
        assert_eq!(
            <Demo as Program>::writer(ctx.shared()),
            Some(Participant::new(1))
        );
        assert!(matches!(
            <Demo as Program>::on_message(
                &mut ctx,
                Participant::new(1),
                Message::Result(transfer::TransferMessage::Failed)
            )
            .unwrap(),
            ApplyDecision::Accept(Transition::End)
        ));
        assert_eq!(<Demo as Program>::writer(ctx.shared()), None);
        let outcome = <Demo as Program>::outcome(ctx.shared());
        assert!(matches!(outcome.input, TransferStatus::Failed));
        assert!(matches!(outcome.result, TransferStatus::Failed));
    }

    #[test]
    fn typed_direct_handler_rejects_malformed_messages_and_ignores_unknown_transfers() {
        // SAFETY: these pure paths neither mutate shared state nor use Host
        // imports; the context holds the initialized shared image.
        let mut ctx =
            unsafe { LocalContext::__new(initialized(), Local::default(), PeerId([1; 32])) };
        let error = <Demo as Program>::on_direct(&mut ctx, Participant::new(1), vec![255], None)
            .unwrap_err();
        assert!(error.to_string().contains("direct message decode failed"));
        let unknown = borsh::to_vec(&DirectMessage::Failed { transfer_id: 99 }).unwrap();
        <Demo as Program>::on_direct(&mut ctx, Participant::new(1), unknown, None).unwrap();
        assert_eq!(ctx.local().input.next_index, 0);
        assert_eq!(ctx.local().result.next_index, 0);
    }

    #[test]
    fn transfer_fields_declare_all_host_capabilities() {
        let capabilities = <Shared as SharedState>::__required_capabilities();
        for capability in [
            Capability::Messaging,
            Capability::Timers,
            Capability::Blobs,
            Capability::Sign {
                schemes: vec![SignScheme::Ed25519],
            },
        ] {
            assert!(capabilities.contains(&capability));
        }
    }
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

#[arena0::data]
pub enum TransferStatus {
    Complete,
    Failed,
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
        shared.input = Transfer::new(
            0,
            Participant::new(0),
            Participant::new(1),
            params.input_hash,
            params.input_length,
            50_000,
            4,
        )?;
        shared.result = Transfer::new(
            1,
            Participant::new(1),
            Participant::new(0),
            params.result_hash,
            params.result_length,
            50_000,
            4,
        )?;
        Ok(())
    }

    fn writer(shared: &Shared) -> Option<Participant> {
        [&shared.input, &shared.result]
            .into_iter()
            .find(|transfer| !transfer.is_complete() && !transfer.is_failed())
            .map(Transfer::sender)
    }

    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<arena0::ProgramTransition<VerifiedTransfer>, ProgramFault> {
        ctx.shared().input.clone().start(ctx);
        ctx.shared().result.clone().start(ctx);
        Ok(Transition::Stay)
    }

    fn on_timer(
        ctx: &mut LocalContext<Shared, Local>,
        timer: TransferTimer,
    ) -> Result<(), ProgramFault> {
        let id = match &timer {
            TransferTimer::Send { transfer_id } | TransferTimer::Resend { transfer_id, .. } => {
                *transfer_id
            }
        };
        match id {
            0 => {
                let transfer = ctx.shared().input.clone();
                if let Some(message) = transfer.on_timer(ctx, |local| &mut local.input, timer)? {
                    let _ = ctx
                        .primitive_output(message)
                        .broadcast_via(&mut ctx.effects(), Message::Input);
                }
            }
            1 => {
                let transfer = ctx.shared().result.clone();
                if let Some(message) = transfer.on_timer(ctx, |local| &mut local.result, timer)? {
                    let _ = ctx
                        .primitive_output(message)
                        .broadcast_via(&mut ctx.effects(), Message::Result);
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
        slice: Option<Attachment>,
    ) -> Result<(), ProgramFault> {
        let id = match &msg {
            DirectMessage::Chunk { transfer_id, .. }
            | DirectMessage::Ack { transfer_id, .. }
            | DirectMessage::Failed { transfer_id } => *transfer_id,
        };
        match id {
            0 => {
                let transfer = ctx.shared().input.clone();
                if let Some(message) =
                    transfer.on_direct(ctx, |local| &mut local.input, from, msg, slice)?
                {
                    let _ = ctx
                        .primitive_output(message)
                        .broadcast_via(&mut ctx.effects(), Message::Input);
                }
            }
            1 => {
                let transfer = ctx.shared().result.clone();
                if let Some(message) =
                    transfer.on_direct(ctx, |local| &mut local.result, from, msg, slice)?
                {
                    let _ = ctx
                        .primitive_output(message)
                        .broadcast_via(&mut ctx.effects(), Message::Result);
                }
            }
            _ => {}
        }
        Ok(())
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
            input: if shared.input.is_complete() {
                TransferStatus::Complete
            } else {
                TransferStatus::Failed
            },
            result: if shared.result.is_complete() {
                TransferStatus::Complete
            } else {
                TransferStatus::Failed
            },
        }
    }
}
