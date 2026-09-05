//! Unit tests for the `#[arena0::program]` expansion.

use super::args::{Arena0ProgramArgs, ParticipantCountArgs};
use super::capabilities::infer_effect_capabilities;
use super::expand_arena0_program_item;
use super::trait_form::{TraitFormAsyncVisitor, expand_arena0_program};
use syn::visit::Visit;
use syn::{Item, ItemImpl};

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
    let error = match syn::parse_str::<Arena0ProgramArgs>(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2..64"#,
    ) {
        Ok(_) => panic!("open participant-count ranges should fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("inclusive bounds"));
}

#[test]
fn rejects_reversed_participant_count_ranges() {
    let error = match syn::parse_str::<Arena0ProgramArgs>(
        r#"name = "test", version = "1.0.0", description = "test", participants = 4..=3"#,
    ) {
        Ok(_) => panic!("reversed participant-count ranges should fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("must not exceed"));
}

#[test]
fn rejects_async_trait_form_handlers() {
    let item: ItemImpl = syn::parse_quote! {
        impl Program for TestProgram {
            type Shared = Shared;
            type Local = ();
            type Message = Vec<u8>;
            type Callout = ();
            type Input = ();
            type Params = ();
            type Query = ();

            async fn on_timer(ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                ctx.effects().set_timer(1, ());
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program(args(), item).unwrap_err();
    assert!(
        err.to_string()
            .contains("async #[arena0::program] trait-form handlers")
    );
}

#[test]
fn rejects_unsupported_program_arguments() {
    let err = match syn::parse_str::<Arena0ProgramArgs>(
        r#"name = "test", version = "1.0.0", description = "test", unsupported = true"#,
    ) {
        Ok(_) => panic!("unsupported argument should fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("unsupported arena0::program argument")
    );
}

#[test]
fn rejects_trait_form_async_blocks() {
    let item: ItemImpl = syn::parse_quote! {
        impl Program for TestProgram {
            type Shared = Shared;
            type Local = ();
            type Message = Vec<u8>;
            type Callout = ();
            type Input = ();
            type Params = ();
            type Query = ();

            fn on_timer(ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                let _future = async {
                    ctx.effects().callout(()).await?;
                    Ok::<(), Error>(())
                };
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program(args(), item).unwrap_err();
    assert!(err.to_string().contains("async blocks are not supported"));
}

#[test]
fn trait_form_async_visitor_rejects_awaits() {
    let expr: syn::Expr = syn::parse_quote! { future.await };
    let mut visitor = TraitFormAsyncVisitor { error: None };

    visitor.visit_expr(&expr);

    let err = visitor.error.unwrap();
    assert!(err.to_string().contains("await is not supported"));
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

            fn initialize(_ctx: &mut Context, _params: ()) -> Result<Transition<Phase>, ProgramFault> {
                Ok(Transition::Stay)
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
}

#[test]
fn module_shell_requires_writer_for_public_messages() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            pub enum Message { Ping }

            fn on_message(
                _ctx: &mut SharedContext,
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
fn trait_form_emits_default_view_export() {
    let item: ItemImpl = syn::parse_quote! {
        impl Program for TestProgram {
            type Shared = Shared;
            type Local = ();
            type Message = Vec<u8>;
            type Callout = ();
            type Input = ();
            type Params = ();
        }
    };

    let expanded = expand_arena0_program(args(), item).unwrap().to_string();

    assert!(expanded.contains("pub extern \"C\" fn arena0_view"));
    assert!(expanded.contains("impl :: arena0 :: ProgramView for TestProgram"));
    assert!(expanded.contains("View :: new"));
    assert!(expanded.contains("format ! (\"{:#?}\" , ctx . shared ())"));
    assert!(expanded.contains("InitInput"));
    assert!(expanded.contains("SharedInput"));
    assert!(expanded.contains("SharedOutput"));
    assert!(expanded.contains("LocalInput"));
    assert!(expanded.contains("LocalOutput"));
    assert!(expanded.contains("QueryOutput"));
    assert!(expanded.contains("ViewOutput"));
    assert!(expanded.contains("OutcomeOutput"));
    assert!(expanded.contains("borsh :: to_vec (shared)"));
    assert!(expanded.contains("borsh :: from_slice (bytes . as_bytes ())"));
    assert!(!expanded.contains("__flush_shared"));
    assert!(!expanded.contains("__restore_shared"));
    assert!(expanded.contains("__phase_decls"));
    assert!(expanded.contains("status_bar"));
}

#[test]
fn trait_form_wires_explicit_view_handler() {
    let item: ItemImpl = syn::parse_quote! {
        impl Program for TestProgram {
            type Shared = Shared;
            type Local = ();
            type Message = Vec<u8>;
            type Callout = ();
            type Input = ();
            type Params = ();

            fn view(_ctx: &Context, _viewport: &Viewport) -> View {
                View::new().header("explicit")
            }
        }
    };

    let expanded = expand_arena0_program(args(), item).unwrap().to_string();

    assert!(expanded.contains("impl :: arena0 :: ProgramView for TestProgram"));
    assert!(expanded.contains("header (\"explicit\")"));
    assert!(!expanded.contains("format ! (\"{:#?}\" , ctx . shared ())"));
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

            fn view(ctx: &Context, viewport: &Viewport) -> View {
                View::new()
                    .header(format!("width {}", viewport.width))
                    .state(format!("{:?}", ctx.shared()))
            }
        }
    };

    let expanded = expand_arena0_program_item(args(), item)
        .unwrap()
        .to_string();

    assert!(expanded.contains("impl :: arena0 :: ProgramView for Ping"));
    assert!(expanded.contains("self :: view (ctx , viewport)"));
    assert!(expanded.contains("pub extern \"C\" fn arena0_view"));
}

#[test]
fn module_shell_lowers_typed_timer_handler() {
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

    assert!(expanded.contains("__arena0_on_typed_timer"));
    assert!(expanded.contains("decode_timer_payload"));
    assert!(expanded.contains("Capability :: Timers"));
}

#[test]
fn module_shell_rejects_external_modules() {
    let item: Item = syn::parse_quote! {
        mod ping;
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("inline module body"));
}

#[test]
fn module_shell_rejects_async_handlers_without_arena_awaits() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            async fn on_timer(ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                ctx.effects().set_timer(1, ());
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("one direct awaited arena effect"));
}

#[test]
fn module_shell_rejects_ordinary_future_awaits() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            async fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                std::future::ready(()).await;
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("direct ctx.effects().callout"));
}

#[test]
fn module_shell_rejects_sleep_awaits() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            async fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("host sleeps are not supported"));
}

#[test]
fn module_shell_rejects_aliased_sleep_awaits() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use tokio::time::sleep;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            async fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                sleep(std::time::Duration::from_millis(1)).await;
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("host sleeps are not supported"));
}

#[test]
fn module_shell_rejects_globbed_sleep_awaits() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use tokio::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            async fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                time::sleep(std::time::Duration::from_millis(1)).await;
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("host sleeps are not supported"));
}

#[test]
fn module_shell_rejects_task_spawning() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                tokio::spawn(worker);
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("task spawning is not supported"));
}

#[test]
fn module_shell_rejects_aliased_task_spawning() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use tokio::spawn as launch;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                launch(worker);
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("task spawning is not supported"));
}

#[test]
fn module_shell_rejects_globbed_task_spawning() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use tokio::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                task::spawn(worker);
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("task spawning is not supported"));
}

#[test]
fn module_shell_allows_similar_user_path_names() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                not_tokio::spawn(worker);
                Ok(Transition::Stay)
            }
        }
    };

    expand_arena0_program_item(args(), item).unwrap();
}

#[test]
fn module_shell_rejects_peer_receive_awaits() {
    let source = r#"
            pub mod ping {
                #[arena0::state(max = 256)]
                pub struct Shared {
                    round: u64,
                }

                async fn on_timer(ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                    ctx.__METHOD__().await;
                    Ok(Transition::Stay)
                }
            }
            "#
    .replace("__METHOD__", "recv");
    let item: Item = syn::parse_str(&source).unwrap();

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(
        err.to_string()
            .contains("awaitable receive is not supported")
    );
}

#[test]
fn module_shell_rejects_timer_awaits() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            async fn on_timer(ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                ctx.timer().await;
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(
        err.to_string()
            .contains("awaitable timers are not supported")
    );
}

#[test]
fn module_shell_rejects_network_io_calls() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                std::net::TcpStream::connect("127.0.0.1:0");
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("network I/O is not supported"));
}

#[test]
fn module_shell_rejects_aliased_network_io_calls() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use std::net::TcpStream as Stream;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                Stream::connect("127.0.0.1:0");
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("network I/O is not supported"));
}

#[test]
fn module_shell_rejects_async_runtime_network_io_calls() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use tokio::net::TcpStream;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                TcpStream::connect("127.0.0.1:0");
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("network I/O is not supported"));
}

#[test]
fn module_shell_rejects_globbed_async_runtime_network_io_calls() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use tokio::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                net::TcpStream::connect("127.0.0.1:0");
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("network I/O is not supported"));
}

#[test]
fn module_shell_rejects_async_std_network_io_calls() {
    let item: Item = syn::parse_quote! {
        pub mod ping {
            use async_std::net::TcpStream as Stream;

            #[arena0::state(max = 256)]
            pub struct Shared {
                round: u64,
            }

            fn on_timer(_ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                Stream::connect("127.0.0.1:0");
                Ok(Transition::Stay)
            }
        }
    };

    let err = expand_arena0_program_item(args(), item).unwrap_err();
    assert!(err.to_string().contains("network I/O is not supported"));
}

#[test]
fn module_shell_rejects_async_shared_message_handler() {
    let item: Item = syn::parse_quote! {
        pub mod chess {
            use arena0::prelude::*;

            #[arena0::callouts]
            pub enum Callout {
                Choose { board: String },
            }

            #[arena0::state(max = 256)]
            pub struct Shared {
                move_text: String,
            }

            async fn on_message(
                ctx: &mut Context,
                _from: Participant,
                _msg: String,
            ) -> Result<Transition<Phase>, ProtocolFault> {
                let move_text = ctx
                    .effects()
                    .callout(callouts::Choose { board: String::new() })
                    .pending("thinking")
                    .await?;
                ctx.mutate_shared(|state| {
                    state.move_text = move_text;
                });
                Ok(Transition::Stay)
            }
        }
    };

    let args: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto)"#,
    )
    .unwrap();
    let err = expand_arena0_program_item(args, item).unwrap_err();
    assert!(err.to_string().contains("async shared handlers"));
}

#[test]
fn module_shell_rejects_pre_await_context_borrow_used_after_resume() {
    let item: Item = syn::parse_quote! {
        pub mod chess {
            use arena0::prelude::*;

            #[arena0::callouts]
            pub enum Callout {
                Choose { board: String },
            }

            #[arena0::state(max = 256)]
            pub struct Shared {
                move_text: String,
            }

            async fn on_timer(ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                let state = ctx.shared_mut();
                let move_text = ctx
                    .effects()
                    .callout(callouts::Choose { board: String::new() })
                    .pending("thinking")
                    .await?;
                state.move_text = move_text;
                Ok(Transition::Stay)
            }
        }
    };

    let args: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto)"#,
    )
    .unwrap();
    let err = expand_arena0_program_item(args, item).unwrap_err();
    assert!(
        err.to_string()
            .contains("not captured across the generated continuation")
    );
    assert!(err.to_string().contains("Local or Shared"));
}

#[test]
fn module_shell_lowers_bare_awaited_effect_statement() {
    let item: Item = syn::parse_quote! {
        pub mod signer {
            use arena0::prelude::*;

            #[arena0::state(max = 256)]
            pub struct Shared {
                done: bool,
            }

            async fn on_timer(ctx: &mut Context) -> Result<Transition<Phase>, ProgramFault> {
                ctx.effects()
                    .sign(SignScheme::Ed25519, b"payload")
                    .pending("signing")
                    .await?;
                ctx.mutate_shared(|state| state.done = true);
                Ok(Transition::Stay)
            }
        }
    };

    let args: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto)"#,
    )
    .unwrap();
    let expanded = expand_arena0_program_item(args, item).unwrap().to_string();

    assert!(expanded.contains("__arena0_resume_on_timer_0"));
    assert!(expanded.contains("SignScheme :: Ed25519"));
    assert!(expanded.contains("Capability :: Sign"));
}

#[test]
fn detects_auto_capability_argument() {
    let args: Arena0ProgramArgs = syn::parse_str(
        r#"name = "test", version = "1.0.0", description = "test", participants = 2, capabilities(auto, Messaging)"#,
    )
    .unwrap();

    assert!(args.capabilities_auto);
}

#[test]
fn infers_direct_effect_capabilities_from_handlers() {
    let item: ItemImpl = syn::parse_quote! {
        impl Program for TestProgram {
            type Shared = Shared;
            type Local = ();
            type Message = Vec<u8>;
            type Callout = ();
            type Input = ();
            type Params = ();
            type Query = ();

            fn initialize(ctx: &mut Context, _params: ()) -> Result<Transition<Phase>, ProgramFault> {
                let mut fx = ctx.effects();
                fx.callout(());
                fx.set_timer(1, ());
                fx.sign(SignScheme::Ed25519, vec![1, 2, 3]);
                Ok(Transition::Stay)
            }

            fn on_input(ctx: &mut Context, _input: ()) -> Result<Transition<Phase>, InputFault> {
                ctx.effects().send(ctx.other(), vec![]);
                Ok(Transition::Stay)
            }
        }
    };

    let inferred = infer_effect_capabilities(&item);
    let rendered = inferred
        .iter()
        .map(|capability| capability.capability.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Capability :: Messaging"));
    assert!(rendered.contains("Capability :: Input"));
    assert!(rendered.contains("Capability :: Timers"));
    assert!(rendered.contains("Capability :: Sign"));
    assert!(rendered.contains("SignScheme :: Ed25519"));
}

#[test]
fn effect_capability_inference_ignores_unrelated_method_names() {
    let item: ItemImpl = syn::parse_quote! {
        impl Program for TestProgram {
            type Shared = Shared;
            type Local = ();
            type Message = Vec<u8>;
            type Callout = ();
            type Input = ();
            type Params = ();
            type Query = ();

            fn initialize(_ctx: &mut Context, _params: ()) -> Result<Transition<Phase>, ProgramFault> {
                formatter.sign();
                mailbox.send(vec![]);
                Ok(Transition::Stay)
            }
        }
    };

    assert!(infer_effect_capabilities(&item).is_empty());
}
