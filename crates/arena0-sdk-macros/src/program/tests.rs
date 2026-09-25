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
                _from: Participant,
                _msg: Message,
            ) -> MessageApply<Ping> {
                ctx.effects().broadcast(&Message::Pong)?;
                Ok(ApplyDecision::Accept(Transition::Stay))
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

            fn on_timer(ctx: &mut LocalContext, timer: Timer) -> Result<(), ProgramFault> {
                let _ = timer;
                ctx.mutate_local(|local| local.fired += 1);
                Ok(())
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

fn auto_args() -> Arena0ProgramArgs {
    syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto)"#,
    )
    .unwrap()
}

fn expand_sign_module(item: Item) -> String {
    expand_arena0_program_item(auto_args(), item)
        .unwrap()
        .to_string()
}

#[test]
fn infers_the_sign_scheme_of_a_literal_context_call() {
    for (scheme, expected, absent) in [
        ("Ed25519", "SignScheme :: Ed25519", "SignScheme :: Bls"),
        ("Bls", "SignScheme :: Bls", "SignScheme :: Ed25519"),
    ] {
        let item: Item = syn::parse_str(&format!(
            r#"
            pub mod ping {{
                use arena0::prelude::*;

                #[arena0::state(max = 256)]
                pub struct Shared {{ round: u64 }}

                fn on_input(
                    ctx: &mut LocalContext<Shared, Local>,
                    _input: Input,
                ) -> arena0::anyhow::Result<()> {{
                    let _ = ctx.sign(SignScheme::{scheme}, b"payload");
                    Ok(())
                }}
            }}
            "#
        ))
        .unwrap();

        let expanded = expand_sign_module(item);
        assert!(expanded.contains("Capability :: Sign"), "{expanded}");
        assert!(expanded.contains(expected), "{expanded}");
        assert!(!expanded.contains(absent), "{expanded}");
    }
}

#[test]
fn infers_sign_through_a_bare_local_context() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared { round: u64 }

            fn on_timer(ctx: &mut LocalContext) -> Result<(), ProgramFault> {
                let _ = ctx.sign(SignScheme::Ed25519, b"payload");
                Ok(())
            }
        }
    };

    let expanded = expand_sign_module(item);
    assert!(expanded.contains("Capability :: Sign"), "{expanded}");
    assert!(expanded.contains("SignScheme :: Ed25519"), "{expanded}");
    assert!(
        expanded.contains("ctx : & mut LocalContext < Shared , () >"),
        "bare LocalContext is rewritten to its generic form: {expanded}"
    );
}

#[test]
fn infers_both_sign_schemes_for_a_dynamic_scheme() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared { round: u64 }

            fn on_input(ctx: &mut LocalContext, _input: Input) -> arena0::anyhow::Result<()> {
                let scheme = SignScheme::Bls;
                let _ = ctx.sign(scheme, b"payload");
                Ok(())
            }
        }
    };

    let expanded = expand_sign_module(item);
    assert!(expanded.contains("SignScheme :: Ed25519"), "{expanded}");
    assert!(expanded.contains("SignScheme :: Bls"), "{expanded}");
}

#[test]
fn ignores_sign_calls_on_non_context_receivers() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared { round: u64 }

            struct Helper;

            impl Helper {
                fn sign(&self, _payload: &[u8]) {}
            }

            fn on_input(ctx: &mut LocalContext, _input: Input) -> arena0::anyhow::Result<()> {
                let helper = Helper;
                helper.sign(b"payload");
                let _ = ctx;
                Ok(())
            }
        }
    };

    let expanded = expand_sign_module(item);
    assert!(!expanded.contains("Capability :: Sign"), "{expanded}");
}

#[test]
fn infers_sign_through_a_context_alias() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared { round: u64 }

            fn on_input(ctx: &mut LocalContext, _input: Input) -> arena0::anyhow::Result<()> {
                let signer = &*ctx;
                let _ = signer.sign(SignScheme::Bls, b"payload");
                Ok(())
            }
        }
    };

    let expanded = expand_sign_module(item);
    assert!(expanded.contains("Capability :: Sign"), "{expanded}");
    assert!(!expanded.contains("SignScheme :: Ed25519"), "{expanded}");
}

#[test]
fn scopes_context_bindings_to_the_declaring_function() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared { round: u64 }

            struct Helper;

            impl Helper {
                fn sign(&self, _scheme: SignScheme, _payload: &[u8]) {}
            }

            fn on_input(ctx: &mut LocalContext, _input: Input) -> arena0::anyhow::Result<()> {
                let _ = ctx;
                Ok(())
            }

            fn helper(ctx: &Helper) {
                ctx.sign(SignScheme::Ed25519, b"payload");
            }
        }
    };

    let expanded = expand_sign_module(item);
    assert!(!expanded.contains("Capability :: Sign"), "{expanded}");
}

#[test]
fn ignores_sign_calls_through_a_shadowing_local() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared { round: u64 }

            struct Helper;

            impl Helper {
                fn sign(&self, _scheme: SignScheme, _payload: &[u8]) {}
            }

            fn on_input(ctx: &mut LocalContext, _input: Input) -> arena0::anyhow::Result<()> {
                let ctx = Helper;
                ctx.sign(SignScheme::Ed25519, b"payload");
                Ok(())
            }
        }
    };

    let expanded = expand_sign_module(item);
    assert!(!expanded.contains("Capability :: Sign"), "{expanded}");
}

#[test]
fn scopes_effect_bindings_to_the_declaring_function() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared { round: u64 }

            pub enum Message { Pong }

            fn on_input(ctx: &mut LocalContext, _input: Input) -> arena0::anyhow::Result<()> {
                let effects = ctx.effects();
                let _ = effects;
                Ok(())
            }

            fn helper(effects: &mut Effects) {
                let _ = effects.broadcast(&Message::Pong);
            }
        }
    };

    let expanded = expand_sign_module(item);
    assert!(!expanded.contains("Capability :: Messaging"), "{expanded}");
}
