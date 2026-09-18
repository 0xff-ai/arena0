//! Resident-compatible guest ABI emission for trait-form programs.
//!
//! This module owns only the generated ABI exports and their state restoration
//! helpers. Trait parsing, associated-type defaults, and handler extraction stay
//! in the sibling trait_form module.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{ItemImpl, Type};

pub(super) struct GuestAbi {
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

pub(super) fn guest_abi(input: GuestAbi) -> TokenStream2 {
    let GuestAbi {
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
            // SAFETY: the guest allocator owns the returned linear-memory
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

        fn __arena0_serialize_shared(shared: &#shared_ty) -> ::std::vec::Vec<u8> {
            let bytes = ::arena0::borsh::to_vec(shared)
                .expect("shared state serialization failed");
            assert!(
                bytes.len() <= __ARENA0_STATE_MAX,
                "shared state exceeds STATE_MAX"
            );
            bytes
        }

        fn __arena0_serialize_local(local: &#local_ty) -> ::std::vec::Vec<u8> {
            let bytes = ::arena0::borsh::to_vec(local)
                .expect("local state serialization failed");
            assert!(
                bytes.len() <= ::arena0::MAX_LOCAL_STATE_BYTES as usize,
                "local state exceeds the ABI state bound"
            );
            bytes
        }

        fn __arena0_load_shared() -> #shared_ty {
            let len = ::arena0::__host_state_len(::arena0::__STATE_KIND_SHARED);
            assert!(len <= __ARENA0_STATE_MAX, "shared state exceeds STATE_MAX");
            let mut bytes = ::std::vec![0u8; len];
            ::arena0::__host_state_read(::arena0::__STATE_KIND_SHARED, &mut bytes);
            ::arena0::borsh::from_slice(&bytes).expect("shared state deserialization failed")
        }

        fn __arena0_restore_shared(bytes: &::arena0::SharedStateBytes) -> #shared_ty {
            ::arena0::borsh::from_slice(bytes.as_bytes())
                .expect("shared state deserialization failed")
        }

        fn __arena0_load_local() -> #local_ty {
            let len = ::arena0::__host_state_len(::arena0::__STATE_KIND_LOCAL);
            assert!(
                len <= ::arena0::MAX_LOCAL_STATE_BYTES as usize,
                "local state exceeds the ABI state bound"
            );
            let mut bytes = ::std::vec![0u8; len];
            ::arena0::__host_state_read(::arena0::__STATE_KIND_LOCAL, &mut bytes);
            ::arena0::borsh::from_slice(&bytes).expect("local state deserialization failed")
        }

        fn __arena0_store_state(
            ctx: ::arena0::Context<#shared_ty, #local_ty>,
        ) {
            let (shared, local, _peer_id) = ctx.__into_parts();
            let shared = __arena0_serialize_shared(&shared);
            let local = __arena0_serialize_local(&local);
            ::arena0::__host_state_write(::arena0::__STATE_KIND_SHARED, &shared);
            ::arena0::__host_state_write(::arena0::__STATE_KIND_LOCAL, &local);
        }

        fn __arena0_make_ctx(
            input: &::arena0::DispatchInput,
        ) -> ::arena0::Context<#shared_ty, #local_ty> {
            let peer_id = ::arena0::types::PeerId(input.peer_id);
            let session: ::arena0::Ensemble<::arena0::Committed> =
                ::arena0::borsh::from_slice(&input.session)
                    .expect("session context deserialization failed");
            let participant = session
                .participant_of(&peer_id)
                .expect("local peer is not in the committed ensemble");
            let mut ctx = ::arena0::Context::__new(
                __arena0_load_shared(),
                __arena0_load_local(),
                peer_id,
            );
            ctx.__set_participant(participant);
            if let Some(remote) = session.others(&peer_id).next() {
                ctx.__set_remote_peer(remote);
            }
            ctx.__set_committed_ensemble(session);
            ctx
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
            // SAFETY: `ptr` and `len` are the unchanged pair returned by
            // `arena0_alloc`, and the Host releases each ABI buffer once.
            unsafe {
                ::arena0::io_alloc::io_dealloc(ptr as *mut u8, len as usize);
            }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_prepare() -> i32 {
            ::arena0::__prepare_allocator() as i32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_initialize(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::InitInput = __arena0_read_input(input_ptr, input_len);
            let mut shared = <#shared_ty as ::core::default::Default>::default();
            let params: #params_ty = ::arena0::serde_json::from_slice(&input.params)
                .expect("params deserialization failed");
            match <#program_ty as ::arena0::Program>::initialize(&mut shared, params) {
                Ok(()) => {}
                Err(::arena0::ProgramFault(error)) => {
                    // Initialization is state-only. The host does not link
                    // effect imports for this call, so a fault traps here.
                    panic!("program initialization failed: {error:#}");
                }
            }
            let local = <#local_ty as ::core::default::Default>::default();
            let shared = ::arena0::SharedStateBytes::try_from(__arena0_serialize_shared(&shared))
                .expect("initialized shared state exceeds ABI bound");
            let local = ::arena0::LocalStateBytes::try_from(__arena0_serialize_local(&local))
                .expect("initialized local state exceeds ABI bound");
            __arena0_write_result(&::arena0::InitializedState {
                shared,
                local,
            })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_dispatch(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::DispatchInput = __arena0_read_input(input_ptr, input_len);
            let raw_event: ::arena0::Event = ::arena0::borsh::from_slice(&input.event)
                .expect("dispatch event deserialization failed");
            let mut ctx = __arena0_make_ctx(&input);
            let mut store_state = true;
            let status = match raw_event {
                ::arena0::Event::SessionStarted { ensemble } => {
                    ctx.__set_committed_ensemble(ensemble.clone());
                    match <#program_ty as ::arena0::Program>::on_session_started(
                        &mut ctx,
                        &ensemble,
                    ) {
                        Ok(transition) => {
                            ctx.__apply_transition::<#program_ty>(transition);
                            ::arena0::CallStatus::Accepted
                        }
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("session-start handler failed: {error:#}");
                        }
                    }
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
                    let from = ctx.participant_for_peer(from);
                    match <#program_ty as ::arena0::Program>::on_message(
                        &mut ctx,
                        from,
                        typed_msg,
                    ) {
                        Ok(::arena0::ApplyDecision::Accept(transition)) => {
                            ctx.__apply_transition::<#program_ty>(transition);
                            ::arena0::CallStatus::Accepted
                        }
                        Ok(::arena0::ApplyDecision::Reject) => ::arena0::CallStatus::Rejected,
                        Err(error) => panic!("message handler failed: {error}"),
                    }
                }
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
                    match <#program_ty as ::arena0::Program>::on_input(&mut ctx, input) {
                        Ok(transition) => {
                            ctx.__apply_transition::<#program_ty>(transition);
                            ::arena0::CallStatus::Accepted
                        }
                        Err(::arena0::InputFault::Unrecoverable(error)) => {
                            panic!("input handler failed: {error:#}");
                        }
                        Err(::arena0::InputFault::Retryable(error)) => {
                            ::arena0::__host_retry_input(&format!("{error:#}"));
                            store_state = false;
                            ::arena0::CallStatus::Accepted
                        }
                    }
                }
                ::arena0::Event::TimerFired => {
                    match <#program_ty as ::arena0::Program>::on_timer(&mut ctx) {
                        Ok(transition) => {
                            ctx.__apply_transition::<#program_ty>(transition);
                            ::arena0::CallStatus::Accepted
                        }
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("timer handler failed: {error:#}");
                        }
                    }
                }
                ::arena0::Event::TypedTimerFired { timer } => {
                    match <#program_ty as ::arena0::Program>::__arena0_on_typed_timer(
                        &mut ctx,
                        timer,
                    ) {
                        Ok(transition) => {
                            ctx.__apply_transition::<#program_ty>(transition);
                            ::arena0::CallStatus::Accepted
                        }
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("typed-timer handler failed: {error:#}");
                        }
                    }
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
                    match <#program_ty as ::arena0::Program>::__arena0_on_signed(
                        &mut ctx,
                        signature,
                    ) {
                        Ok(transition) => {
                            ctx.__apply_transition::<#program_ty>(transition);
                            ::arena0::CallStatus::Accepted
                        }
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("signed handler failed: {error:#}");
                        }
                    }
                }
                ::arena0::Event::React => {
                    match <#program_ty as ::arena0::Program>::on_react(&mut ctx) {
                        Ok(transition) => {
                            ctx.__apply_transition::<#program_ty>(transition);
                            ::arena0::CallStatus::Accepted
                        }
                        Err(::arena0::ProgramFault(error)) => {
                            panic!("react handler failed: {error:#}");
                        }
                    }
                }
            };
            if store_state && status == ::arena0::CallStatus::Accepted {
                __arena0_store_state(ctx);
            }
            __arena0_write_result(&::arena0::DispatchOutput { status })
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn arena0_query(input_ptr: i32, input_len: i32) -> i64 {
            let input: ::arena0::QueryInput = __arena0_read_input(input_ptr, input_len);
            let query: #query_ty = ::arena0::serde_json::from_slice(&input.query)
                .expect("query deserialization failed");
            let shared = __arena0_restore_shared(&input.shared);
            let ensemble: ::arena0::Ensemble<::arena0::Committed> =
                ::arena0::borsh::from_slice(&input.session)
                    .expect("query session context deserialization failed");
            let response = <#program_ty as ::arena0::ProgramQuery>::query(
                &shared,
                &ensemble,
                query,
            );
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
            let shared = __arena0_restore_shared(&input.shared);
            let ensemble: ::arena0::Ensemble<::arena0::Committed> =
                ::arena0::borsh::from_slice(&input.session)
                    .expect("view session context deserialization failed");
            let view = <#program_ty as ::arena0::ProgramView>::view(
                &shared,
                &ensemble,
                &viewport,
            );
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
            let shared = __arena0_restore_shared(&input.shared);
            let outcome = <#program_ty as ::arena0::Program>::outcome(&shared);
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
