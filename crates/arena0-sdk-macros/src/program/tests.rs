//! Unit tests for the inline-module `#[arena0::program]` expansion.

use super::args::{Arena0ProgramArgs, ParticipantCountArgs};
use super::expand_arena0_program_item;
use syn::Item;

fn args() -> Arena0ProgramArgs {
    syn::parse_str(r#"name = "test", version = "1.0.0", description = "test", participants = 2"#)
        .unwrap()
}

#[test]
fn parses_exact_and_inclusive_participant_counts() {
    let exact: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2"#,
    )
    .unwrap();
    assert!(matches!(exact.participants, ParticipantCountArgs::Exact(_)));

    let ranged: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2..=64"#,
    )
    .unwrap();
    let ParticipantCountArgs::Range { min, max } = ranged.participants else {
        panic!("expected inclusive participant-count range");
    };
    assert_eq!(min.base10_parse::<u8>().unwrap(), 2);
    assert_eq!(max.base10_parse::<u8>().unwrap(), 64);
}

#[test]
fn requires_participants() {
    let error = syn::parse_str::<Arena0ProgramArgs>(
        r#"name = "test", version = "1.0.0", description = "test""#,
    )
    .err()
    .expect("missing participants should fail");
    assert!(error.to_string().contains("missing participants"));
}

#[test]
fn rejects_open_participant_count_ranges() {
    let error = syn::parse_str::<Arena0ProgramArgs>(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2..64"#,
    )
    .err()
    .expect("open participant-count range should fail");
    assert!(error.to_string().contains("inclusive bounds"));
}

#[test]
fn rejects_reversed_participant_count_ranges() {
    let error = syn::parse_str::<Arena0ProgramArgs>(
        r#"name = "test", version = "1.0.0", description = "test", participants = 4..=3"#,
    )
    .err()
    .expect("reversed participant-count range should fail");
    assert!(error.to_string().contains("must not exceed"));
}

#[test]
fn rejects_impl_program_form() {
    let item: Item = syn::parse_quote! {
        impl Program for TestProgram {}
    };

    let error = expand_arena0_program_item(args(), item).unwrap_err();
    assert_eq!(
        error.to_string(),
        "arena0::program annotates an inline module shell"
    );
}

#[test]
fn rejects_unsupported_program_arguments() {
    let error = syn::parse_str::<Arena0ProgramArgs>(
        r#"name = "test", version = "1.0.0", description = "test", unsupported = true"#,
    )
    .err()
    .expect("unsupported argument should fail");
    assert!(
        error
            .to_string()
            .contains("unsupported arena0::program argument")
    );
}

#[test]
fn module_shell_generates_program_struct_and_impl() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            pub enum Message {
                Pong,
            }

            fn initialize(_shared: &mut Shared, _params: ()) -> Result<(), ProgramFault> {
                Ok(())
            }

            fn writer(_shared: &Shared) -> Option<Participant> {
                Some(Participant::new(0))
            }

            fn on_message(
                ctx: &mut Context,
                from: Participant,
                _msg: Message,
            ) -> Result<Transition<Phase>, ProtocolFault> {
                ctx.effects().send(from, &Message::Pong);
                Ok(Transition::Stay)
            }
        }
    };

    let args: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto)"#,
    )
    .unwrap();
    let expanded = expand_arena0_program_item(args, item).unwrap().to_string();

    assert!(expanded.contains("pub struct Ping"));
    assert!(expanded.contains("pub type ProgramContext"));
    assert!(expanded.contains("Generated arena0 program type for the `ping` module shell"));
    assert!(expanded.contains("Context alias for `Ping` handlers"));
    assert!(expanded.contains("impl :: arena0 :: Program for Ping"));
    assert!(expanded.contains("impl :: arena0 :: ProgramQuery for Ping"));
    assert!(expanded.contains("impl :: arena0 :: ProgramView for Ping"));
    assert!(expanded.contains("type Shared = Shared"));
    assert!(expanded.contains("fn initialize"));
    assert!(expanded.contains("fn writer"));
    assert!(expanded.contains("Capability :: Messaging"));
    assert!(expanded.contains("pub use ping :: *"));
    assert!(!expanded.contains("__Arena0Local"));
}

#[test]
fn module_shell_requires_writer_for_program_messages() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            pub enum Message { Ping }

            fn on_message(
                _ctx: &mut Context,
                _from: Participant,
                _message: Message,
            ) -> MessageApply<Ping> {
                Ok(ApplyDecision::Accept(Transition::Stay))
            }
        }
    };

    let error = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(error.to_string().contains("must define writer"));
}

#[test]
fn module_shell_wires_explicit_view_handler() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn view(shared: &Shared, ensemble: &Ensemble, viewport: &Viewport) -> View {
                View::new()
                    .header(format!("width {}", viewport.width))
                    .state(format!("{:?} ({})", shared, ensemble.len()))
            }
        }
    };

    let expanded = expand_arena0_program_item(args(), item)
        .unwrap()
        .to_string();

    assert!(expanded.contains("impl :: arena0 :: ProgramView for Ping"));
    assert!(expanded.contains("self :: view (shared , ensemble , viewport)"));
    assert!(expanded.contains("pub extern \"C\" fn arena0_view"));
}

#[test]
fn module_shell_wires_typed_timer_handler() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            pub enum Timer {
                Ping,
            }

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_session_started(
                ctx: &mut Context,
                _ensemble: &Ensemble,
            ) -> Result<Transition<Phase>, ProgramFault> {
                ctx.effects().set_timer(Timer::Ping, std::time::Duration::from_secs(1));
                Ok(Transition::Stay)
            }

            fn on_timer(ctx: &mut Context, timer: Timer) -> Result<Transition<Phase>, ProgramFault> {
                let _ = timer;
                ctx.mutate_shared(|state| state.round += 1);
                Ok(Transition::Stay)
            }
        }
    };

    let args: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto)"#,
    )
    .unwrap();
    let expanded = expand_arena0_program_item(args, item).unwrap().to_string();

    assert!(expanded.contains("fn on_timer"));
    assert!(expanded.contains("decode_timer_payload"));
    assert!(expanded.contains("Capability :: Timers"));
}

#[test]
fn module_shell_rejects_external_modules() {
    let item: Item = syn::parse_quote! {
        mod ping;
    };

    let error = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(error.to_string().contains("inline module body"));
}

#[test]
fn detects_auto_capability_argument() {
    let args: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto, Messaging)"#,
    )
    .unwrap();

    assert!(args.capabilities_auto);
}
