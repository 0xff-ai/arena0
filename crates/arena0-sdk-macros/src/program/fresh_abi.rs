//! Fresh-instance guest ABI emission for trait-form programs.
//!
//! This module owns only the generated ABI exports and their state restoration
//! helpers. Trait parsing, associated-type defaults, and handler extraction stay
//! in the sibling trait_form module.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{ItemImpl, Type};

pub(super) struct FreshGuestAbi {
    pub(super) item: ItemImpl,
    pub(super) query_impl: TokenStream2,
    pub(super) view_impl: TokenStream2,
    pub(super) program_ty: Box<Type>,
    pub(super) shared_ty: Type,
    pub(super) local_ty: Type,
    pub(super) callout_ty: Type,
    pub(super) message_ty: Type,
    pub(super) params_ty: Type,
    pub(super) outcome_ty: Type,
    pub(super) query_ty: Type,
    pub(super) name: syn::LitStr,
    pub(super) version: syn::LitStr,
    pub(super) description: syn::LitStr,
    pub(super) display_name: syn::LitStr,
    pub(super) participants: TokenStream2,
    pub(super) capabilities: TokenStream2,
    pub(super) inferred_effect_capability_tokens: Vec<TokenStream2>,
}

pub(super) fn fresh_guest_abi(input: FreshGuestAbi) -> TokenStream2 {
    let FreshGuestAbi {
        item,
        query_impl,
        view_impl,
        program_ty,
        shared_ty,
        local_ty,
        callout_ty,
        message_ty,
        params_ty,
        outcome_ty,
        query_ty,
        name,
        version,
        description,
        display_name,
        participants,
        capabilities,
        inferred_effect_capability_tokens,
    } = input;

    quote! {
        #item
        #query_impl
        #view_impl

        #[cfg(target_arch = "wasm32")]
        const _: () = {
        static mut __ARENA0_SHARED: ::core::option::Option<#shared_ty> = None;
        static mut __ARENA0_LOCAL: ::core::option::Option<#local_ty> = None;
        static mut __ARENA0_REMOTE_PEER: ::core::option::Option<::arena0::types::PeerId> = None;
        static mut __ARENA0_PARTICIPANT: ::core::option::Option<::arena0::Participant> = None;
        static mut __ARENA0_ENSEMBLE:
            ::core::option::Option<::arena0::Ensemble<::arena0::Committed>> = None;

        const __ARENA0_STATE_MAX: usize = <#shared_ty as ::arena0::SharedState>::STATE_MAX;

        fn __arena0_pack(ptr: i32, len: i32) -> i64 {
            ((ptr as i64) << 32) | ((len as i64) & 0xFFFF_FFFF)
        }

        fn __arena0_write_result<T: ::arena0::borsh::BorshSerialize>(result: &T) -> i64 {
            let bytes = ::arena0::borsh::to_vec(result)
                .expect("guest call result serialization failed");
            assert!(
                bytes.len() <= ::arena0::MAX_CALL_ENVELOPE_BYTES as usize,
                "guest call result exceeds ABI envelope bound"
            );
            let len = ::core::convert::TryFrom::try_from(bytes.len())
                .expect("guest call result length overflows i32");
            if bytes.is_empty() {
                return __arena0_pack(0, 0);
            }
            // SAFETY: the fresh guest allocator owns the returned linear-memory
            // range until the host reads and deallocates it.
            let ptr = unsafe { arena0_alloc(len) };
            assert!(ptr > 0, "guest result allocation failed");
            unsafe {
                ::core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            }
            __arena0_pack(ptr, len)
        }

        fn __arena0_read_input<T: ::arena0::borsh::BorshDeserialize>(ptr: i32, len: i32) -> T {
            assert!(ptr >= 0 && len >= 0, "negative guest input pointer or length");
            assert!(
                (len as usize) <= ::arena0::MAX_CALL_ENVELOPE_BYTES as usize,
                "guest input envelope exceeds ABI bound"
            );
            // SAFETY: the host writes one bounded call-specific envelope before
            // invoking the export; its decoder checks every variable prefix.
            let bytes = unsafe {
                ::core::slice::from_raw_parts(ptr as *const u8, len as usize)
            };
            ::arena0::borsh::from_slice(bytes).expect("guest call input deserialization failed")
        }

        fn __arena0_flush_shared(shared: &#shared_ty) -> ::arena0::SharedStateBytes {
            let bytes = ::arena0::borsh::to_vec(shared)
                .expect("shared state serialization failed");
            assert!(
                bytes.len() <= __ARENA0_STATE_MAX,
                "shared state exceeds STATE_MAX"
            );
            ::arena0::SharedStateBytes::try_from(bytes)
                .expect("shared state exceeds the ABI state bound")
        }

        fn __arena0_restore_shared(
            bytes: &::arena0::SharedStateBytes,
        ) -> #shared_ty {
            ::arena0::borsh::from_slice(bytes.as_bytes())
                .expect("shared state deserialization failed")
        }

        fn __arena0_flush_local(local: &#local_ty) -> ::arena0::LocalStateBytes {
            let bytes = ::arena0::borsh::to_vec(local)
                .expect("local state serialization failed");
            ::arena0::LocalStateBytes::try_from(bytes)
                .expect("local state exceeds the ABI state bound")
        }

        fn __arena0_result_from_ctx(
            status: ::arena0::CallStatus,
            ctx: ::arena0::Context<#shared_ty, #local_ty>,
        ) -> i64 {
            let (_shared, local, _peer_id) = ctx.__into_parts();
            __arena0_write_result(&::arena0::LocalOutput {
                status,
                local: __arena0_flush_local(&local),
            })
        }

        fn __arena0_shared_result(status: ::arena0::CallStatus, shared: ::arena0::SharedStateBytes) -> i64 {
            __arena0_write_result(&::arena0::SharedOutput { status, shared })
        }

        fn __arena0_set_statics(
            shared: #shared_ty,
            local: #local_ty,
            session: ::core::option::Option<::arena0::Ensemble<::arena0::Committed>>,
            peer_id: ::arena0::types::PeerId,
        ) {
            let (participant, remote) = session.as_ref().map_or((None, None), |ensemble| {
                let participant = ensemble
                    .participant_of(&peer_id)
                    .expect("local peer is not in the committed ensemble");
                (Some(participant), ensemble.others(&peer_id).next())
            });
            // SAFETY: this guest is single-threaded and is dropped after one
            // semantic invocation.
            unsafe {
                ::core::ptr::write(::core::ptr::addr_of_mut!(__ARENA0_SHARED), Some(shared));
                ::core::ptr::write(::core::ptr::addr_of_mut!(__ARENA0_LOCAL), Some(local));
                ::core::ptr::write(::core::ptr::addr_of_mut!(__ARENA0_ENSEMBLE), session);
                ::core::ptr::write(
                    ::core::ptr::addr_of_mut!(__ARENA0_PARTICIPANT),
                    participant,
                );
                ::core::ptr::write(
                    ::core::ptr::addr_of_mut!(__ARENA0_REMOTE_PEER),
                    remote,
                );
            }
        }

        fn __arena0_restore_local(input: &::arena0::LocalInput) {
            let shared: #shared_ty = ::arena0::borsh::from_slice(input.shared.as_bytes())
                .expect("shared state deserialization failed");
            let local: #local_ty = ::arena0::borsh::from_slice(input.local.as_bytes())
                .expect("local state deserialization failed");
            let session: ::arena0::Ensemble<::arena0::Committed> =
                ::arena0::borsh::from_slice(&input.session)
                    .expect("session context deserialization failed");
            __arena0_set_statics(
                shared,
                local,
                Some(session),
                ::arena0::types::PeerId(input.peer_id),
            );
        }

        fn __arena0_shared_parts(
            input: &::arena0::SharedInput,
        ) -> (#shared_ty, ::core::option::Option<::arena0::Ensemble<::arena0::Committed>>) {
            let shared: #shared_ty = ::arena0::borsh::from_slice(input.shared.as_bytes())
                .expect("shared state deserialization failed");
            let session: ::core::option::Option<::arena0::Ensemble<::arena0::Committed>> =
                ::arena0::borsh::from_slice(&input.session)
                    .expect("session context deserialization failed");
            (shared, session)
        }

        fn __arena0_make_shared_ctx(
            input: &::arena0::SharedInput,
        ) -> ::arena0::SharedContext<#shared_ty> {
            let (shared, ensemble) = __arena0_shared_parts(input);
            ::arena0::SharedContext::__new(shared, ensemble)
        }

        fn __arena0_make_projection_ctx(
            shared_bytes: &::arena0::SharedStateBytes,
            session_bytes: &[u8],
        ) -> ::arena0::SharedContext<#shared_ty> {
            let shared: #shared_ty = ::arena0::borsh::from_slice(shared_bytes.as_bytes())
                .expect("shared state deserialization failed");
            let session: ::arena0::Ensemble<::arena0::Committed> =
                ::arena0::borsh::from_slice(session_bytes)
                    .expect("session context deserialization failed");
            ::arena0::SharedContext::__new(shared, Some(session))
        }

        fn __arena0_init_statics(peer_id: ::arena0::types::PeerId) {
            __arena0_set_statics(
                <#shared_ty as ::core::default::Default>::default(),
                <#local_ty as ::core::default::Default>::default(),
                None,
                peer_id,
            );
        }

        fn __arena0_make_ctx(
            peer_id: ::arena0::types::PeerId,
        ) -> ::arena0::Context<#shared_ty, #local_ty> {
            // SAFETY: input restoration or initialization populated both
            // statics before this function is called.
            unsafe {
                let shared = (*::core::ptr::addr_of_mut!(__ARENA0_SHARED))
                    .take()
                    .expect("shared state not initialized");
                let local = (*::core::ptr::addr_of_mut!(__ARENA0_LOCAL))
                    .take()
                    .expect("local state not initialized");
                let mut ctx = ::arena0::Context::__new(shared, local, peer_id);
                if let Some(remote) = *::core::ptr::addr_of!(__ARENA0_REMOTE_PEER) {
                    ctx.__set_remote_peer(remote);
                }
                if let Some(participant) = *::core::ptr::addr_of!(__ARENA0_PARTICIPANT) {
                    ctx.__set_participant(participant);
                }
                if let Some(ensemble) = (*::core::ptr::addr_of!(__ARENA0_ENSEMBLE)).as_ref() {
                    ctx.__set_committed_ensemble(ensemble.clone());
                }
                ctx
            }
        }

        fn __arena0_capture_ensemble(
            ctx: &mut ::arena0::SharedContext<#shared_ty>,
            ensemble: &::arena0::Ensemble<::arena0::Committed>,
        ) {
            ctx.__set_ensemble(ensemble.clone());
        }

        fn __arena0_resolve_participant(
            ctx: &::arena0::SharedContext<#shared_ty>,
            peer: ::arena0::types::PeerId,
        ) -> ::arena0::Participant {
            ctx.participant_for_peer(peer)
        }

        #[unsafe(no_mangle)]
        pub static arena0_abi_version: i32 = ::arena0::ABI_VERSION as i32;

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn arena0_alloc(len: i32) -> i32 {
            if len <= 0 {
                return 0;
            }
            ::arena0::io_alloc::io_alloc(len as usize) as i32
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn arena0_dealloc(ptr: i32, len: i32) {
            if ptr == 0 || len <= 0 {
                return;
            }
            ::arena0::io_alloc::io_dealloc(ptr as *mut u8, len as usize);
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_initialize(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::InitInput = __arena0_read_input(input_ptr, input_len);
            let mut ctx = ::arena0::SharedContext::__new(
                <#shared_ty as ::core::default::Default>::default(),
                None,
            );
            let params: #params_ty = ::arena0::serde_json::from_slice(&input.params)
                .expect("params deserialization failed");
            match <#program_ty as ::arena0::Program>::initialize(&mut ctx, params) {
                Ok(()) => {}
                Err(::arena0::ProgramFault(error)) => {
                    // Initialization is state-only. The host does not link
                    // effect imports for this call, so a fault traps here.
                    panic!("program initialization failed: {error:#}");
                }
            }
            let shared = ctx.__into_shared();
            let local = <#local_ty as ::core::default::Default>::default();
            __arena0_write_result(&::arena0::InitializedState {
                shared: __arena0_flush_shared(&shared),
                local: __arena0_flush_local(&local),
            })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_shared(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::SharedInput = __arena0_read_input(input_ptr, input_len);
            let raw_event: ::arena0::Event = ::arena0::borsh::from_slice(&input.event)
                .expect("shared event deserialization failed");
            assert!(
                matches!(
                    &raw_event,
                    ::arena0::Event::SessionStarted { .. }
                        | ::arena0::Event::MessageReceived { .. }
                ),
                "local event supplied to shared export"
            );
            let mut ctx = __arena0_make_shared_ctx(&input);
            let status = ::arena0::CallStatus::Accepted;

            macro_rules! __shared_handler {
                ($expr:expr) => {{
                    let result = $expr;
                    match result {
                        Ok(transition) => ctx.__apply_transition::<#program_ty>(transition),
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("shared handler failed: {error:#}");
                        }
                    }
                }};
            }

            match raw_event {
                ::arena0::Event::SessionStarted { ensemble } => {
                    __arena0_capture_ensemble(&mut ctx, &ensemble);
                    __shared_handler!(
                        <#program_ty as ::arena0::Program>::on_session_started(
                            &mut ctx,
                            &ensemble,
                        )
                    );
                }
                ::arena0::Event::MessageReceived {
                    message_id: _,
                    from,
                    position: _,
                    pre_state: _,
                    msg,
                } => {
                    let typed_msg: #message_ty = ::arena0::borsh::from_slice(&msg)
                        .expect("message deserialization failed");
                    let from = __arena0_resolve_participant(&ctx, from);
                    let (shared, ensemble) = __arena0_shared_parts(&input);
                    let mut shared_ctx = ::arena0::SharedContext::__new(shared, ensemble);
                    let decision = <#program_ty as ::arena0::Program>::on_message(
                        &mut shared_ctx,
                        from,
                        typed_msg,
                    );
                    match decision {
                        Ok(::arena0::ApplyDecision::Accept(transition)) => {
                            shared_ctx.__apply_transition::<#program_ty>(transition);
                        }
                        Ok(::arena0::ApplyDecision::Reject) => {
                            return __arena0_shared_result(
                                ::arena0::CallStatus::Rejected,
                                input.shared,
                            );
                        }
                        Err(error) => panic!("shared message handler failed: {error:#}"),
                    }
                    let shared = shared_ctx.__into_shared();
                    return __arena0_shared_result(status, __arena0_flush_shared(&shared));
                }
                _ => unreachable!("shared event classification changed"),
            }
            let shared = ctx.__into_shared();
            __arena0_shared_result(status, __arena0_flush_shared(&shared))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_local(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::LocalInput = __arena0_read_input(input_ptr, input_len);
            __arena0_restore_local(&input);
            let raw_event: ::arena0::Event = ::arena0::borsh::from_slice(&input.event)
                .expect("local event deserialization failed");
            assert!(
                matches!(
                    &raw_event,
                    ::arena0::Event::InputReceived { .. }
                        | ::arena0::Event::TimerFired
                        | ::arena0::Event::TypedTimerFired { .. }
                        | ::arena0::Event::Signed { .. }
                        | ::arena0::Event::React
                ),
                "shared event supplied to local export"
            );
            let peer_id = ::arena0::types::PeerId(input.peer_id);
            let mut ctx = __arena0_make_ctx(peer_id);
            let status = ::arena0::CallStatus::Accepted;

            macro_rules! __local_transition {
                ($expr:expr) => {{
                    let result = $expr;
                    match result {
                        Ok(()) => {}
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("local handler failed: {error:#}");
                        }
                    }
                }};
            }

            match raw_event {
                ::arena0::Event::InputReceived {
                    callout_index,
                    data,
                    continuation_tag,
                } => {
                    if let Some(tag) = continuation_tag {
                        <#program_ty as ::arena0::Program>::__arena0_restore_continuation(
                            &mut ctx,
                            tag,
                        );
                    }
                    let input = <#callout_ty as ::arena0::Arena0Callout>::from_raw(
                        callout_index,
                        data,
                    );
                    let result = <#program_ty as ::arena0::Program>::on_input(&mut ctx, input);
                    match result {
                        Ok(()) => {}
                        Err(::arena0::InputFault::Unrecoverable(error)) => {
                            panic!("input handler failed: {error:#}");
                        }
                        Err(::arena0::InputFault::Retryable(error)) => {
                            ::arena0::__host_retry_input(&format!("{error:#}"));
                        }
                    }
                }
                ::arena0::Event::TimerFired => {
                    __local_transition!(<#program_ty as ::arena0::Program>::on_timer(&mut ctx));
                }
                ::arena0::Event::TypedTimerFired { timer } => {
                    __local_transition!(
                        <#program_ty as ::arena0::Program>::__arena0_on_typed_timer(
                            &mut ctx,
                            timer,
                        )
                    );
                }
                ::arena0::Event::Signed {
                    signature,
                    continuation_tag,
                } => {
                    if let Some(tag) = continuation_tag {
                        <#program_ty as ::arena0::Program>::__arena0_restore_continuation(
                            &mut ctx,
                            tag,
                        );
                    }
                    __local_transition!(
                        <#program_ty as ::arena0::Program>::__arena0_on_signed(
                            &mut ctx,
                            signature,
                        )
                    );
                }
                ::arena0::Event::React => {
                    let result = <#program_ty as ::arena0::Program>::on_react(&mut ctx);
                    match result {
                        Ok(()) => {}
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("react handler failed: {error:#}");
                        }
                    }
                }
                _ => unreachable!("local event classification changed"),
            }
            __arena0_result_from_ctx(status, ctx)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_query(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::QueryInput = __arena0_read_input(input_ptr, input_len);
            let query: #query_ty = ::arena0::serde_json::from_slice(&input.query)
                .expect("query deserialization failed");
            let ctx = __arena0_make_projection_ctx(&input.shared, &input.session);
            let response = <#program_ty as ::arena0::ProgramQuery>::query(&ctx, query);
            let json = ::arena0::serde_json::to_vec(&response)
                .expect("query response serialization failed");
            __arena0_write_result(&::arena0::QueryOutput {
                query_index: input.query_index,
                json,
            })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_view(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::ViewInput = __arena0_read_input(input_ptr, input_len);
            let viewport: ::arena0::Viewport = ::arena0::serde_json::from_slice(&input.viewport)
                .expect("viewport deserialization failed");
            let ctx = __arena0_make_projection_ctx(&input.shared, &input.session);
            let view = <#program_ty as ::arena0::ProgramView>::view(&ctx, &viewport);
            let json = ::arena0::serde_json::to_vec(&view)
                .expect("view serialization failed");
            __arena0_write_result(&::arena0::ViewOutput { json })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_writer(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::WriterInput = __arena0_read_input(input_ptr, input_len);
            let shared = __arena0_restore_shared(&input.shared);
            let participant = <#program_ty as ::arena0::Program>::writer(&shared)
                .map(::arena0::Participant::as_u8);
            __arena0_write_result(&::arena0::WriterOutput { participant })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_outcome(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::OutcomeInput = __arena0_read_input(input_ptr, input_len);
            let ctx = __arena0_make_projection_ctx(&input.shared, &input.session);
            let outcome = <#program_ty as ::arena0::Program>::outcome(ctx.shared());
            let borsh = ::arena0::borsh::to_vec(&outcome)
                .expect("outcome Borsh serialization failed");
            let json = ::arena0::serde_json::to_vec(&outcome)
                .expect("outcome serialization failed");
            __arena0_write_result(&::arena0::OutcomeOutput { borsh, json })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_metadata() -> i64 {
            let declared_capabilities: ::std::vec::Vec<::arena0::Capability> =
                ::arena0::__arena0_capability_vec!(#capabilities);
            let mut capabilities = ::arena0::CapabilitySet::new();
            capabilities.extend(declared_capabilities);
            #(
                capabilities.insert(#inferred_effect_capability_tokens);
            )*
            capabilities.extend(<#shared_ty as ::arena0::SharedState>::__required_capabilities());
            let metadata = ::arena0::ProgramMetadata {
                name: #name.into(),
                version: #version.into(),
                description: #description.into(),
                author: None,
                capabilities: capabilities.into_vec(),
                display_name: #display_name.into(),
                participants: #participants,
            };
            let schema = ::arena0::ProgramSchema {
                state: ::arena0::StateSchema {
                    schema: <#shared_ty as ::arena0::ProgramValue>::json_schema(),
                    max_bytes: <#shared_ty as ::arena0::SharedState>::STATE_MAX as u32,
                },
                callouts: <#callout_ty as ::arena0::Arena0Callout>::schemas(),
                messages: ::std::vec![::arena0::MessageSchema {
                    borsh: ::arena0::BorshSchemaDocument::for_type::<#message_ty>(),
                }],
                params: <#params_ty as ::arena0::ProgramValue>::json_schema(),
                queries: <#query_ty as ::arena0::Arena0Query>::schemas(),
                outcome: <#outcome_ty as ::arena0::ProgramValue>::json_schema(),
            };
            let bytes = ::arena0::ProgramDefinition { metadata, schema }
                .encode()
                .expect("metadata serialization failed");
            if bytes.is_empty() {
                return __arena0_pack(0, 0);
            }
            let len = ::core::convert::TryFrom::try_from(bytes.len())
                .expect("metadata length overflows i32");
            let ptr = unsafe { arena0_alloc(len) };
            assert!(ptr > 0, "metadata allocation failed");
            unsafe {
                ::core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            }
            __arena0_pack(ptr, len)
        }
        };
    }
}
