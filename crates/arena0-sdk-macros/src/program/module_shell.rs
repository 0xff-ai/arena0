//! Expansion of the inline-module (`mod` shell) form of `#[arena0::program]`.
//!
//! Owns `expand_arena0_program_module` and its supporting codegen:
//! generated `Program`/`ProgramQuery`/`ProgramView` impl assembly, associated-
//! type and handler-method synthesis, and the module-item query/view and
//! bare-`Context` rewrite helpers. This is the module-shell codegen seam.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Error, Ident, Item, ItemFn, ItemMod, Result, Type, spanned::Spanned};

use super::args::Arena0ProgramArgs;
use super::capabilities::{InferredEffectCapability, infer_effect_capabilities_from_module};
use super::guest_abi::{GuestAbi, guest_abi};
use super::{rewrite_context_type, to_pascal_case};

#[allow(clippy::too_many_lines)]
pub(super) fn expand_arena0_program_module(
    args: Arena0ProgramArgs,
    mut module: ItemMod,
) -> Result<TokenStream2> {
    let module_attrs = module.attrs.clone();
    let module_vis = module.vis.clone();
    let mod_ident = module.ident.clone();
    let program_ident = format_ident!("{}", to_pascal_case(&mod_ident.to_string()));
    let Some((_, items)) = module.content.as_mut() else {
        return Err(Error::new(
            module.span(),
            "arena0::program module shells must use an inline module body",
        ));
    };

    let shared_ty: Type = if module_has_item_type(items, "Shared") {
        syn::parse_quote!(Shared)
    } else if module_has_item_type(items, "State") {
        syn::parse_quote!(State)
    } else {
        return Err(Error::new(
            module.span(),
            "arena0::program module shells need a Shared or State type",
        ));
    };
    let user_local_ty: Type = if module_has_item_type(items, "Local") {
        syn::parse_quote!(Local)
    } else {
        syn::parse_quote!(())
    };

    if module_has_fn(items, "on_message") && !module_has_fn(items, "writer") {
        return Err(Error::new(
            module.span(),
            "arena0::program modules with on_message must define writer(shared) -> Option<Participant> for agreed messages",
        ));
    }
    let local_ty = user_local_ty.clone();
    rewrite_bare_context_in_module(items, &shared_ty, &local_ty);

    let message_ty: Type = if module_has_item_type(items, "Message") {
        syn::parse_quote!(Message)
    } else {
        syn::parse_quote!(Vec<u8>)
    };
    let callout_ty: Type = if module_has_item_type(items, "Callout") {
        syn::parse_quote!(Callout)
    } else {
        syn::parse_quote!(())
    };
    let input_ty: Type = if module_has_item_type(items, "Input") {
        syn::parse_quote!(Input)
    } else {
        syn::parse_quote!(<#callout_ty as ::arena0::Arena0Callout>::Response)
    };
    let params_ty: Type = if module_has_item_type(items, "Params") {
        syn::parse_quote!(Params)
    } else {
        syn::parse_quote!(())
    };
    let outcome_ty: Type = if module_has_item_type(items, "Outcome") {
        syn::parse_quote!(Outcome)
    } else {
        syn::parse_quote!(())
    };
    let query_ty: Type = if module_has_item_type(items, "Query") {
        syn::parse_quote!(Query)
    } else {
        syn::parse_quote!(())
    };
    let assoc_types = module_assoc_types(
        &shared_ty,
        &local_ty,
        &message_ty,
        &callout_ty,
        &input_ty,
        &params_ty,
        &outcome_ty,
    );
    let methods = module_handler_methods(items);
    let program_impl = quote! {
        impl ::arena0::Program for #program_ident {
            #(#assoc_types)*
            #(#methods)*
        }
    };
    let query_impl = module_query_impl(items, &program_ident);
    let view_impl = module_view_impl(items, &program_ident, &shared_ty);
    let program_types = ProgramTypes {
        shared: shared_ty.clone(),
        local: local_ty.clone(),
        callout: callout_ty,
        message: message_ty,
        params: params_ty,
        outcome: outcome_ty,
        query: query_ty,
    };
    let inferred_effect_capabilities = if args.capabilities_auto {
        infer_effect_capabilities_from_module(items)
    } else {
        Vec::new()
    };
    let expanded_impl = expand_arena0_program_with_inferred(
        args,
        &program_ident,
        program_impl,
        view_impl,
        program_types,
        inferred_effect_capabilities,
    )?;
    let module_items = items.iter();
    let program_doc = format!(
        "Generated arena0 program type for the `{}` module shell.",
        mod_ident
    );
    let context_doc = format!(
        "Context alias for `{}` handlers, using this program's shared and local state.",
        program_ident
    );

    Ok(quote! {
        #(#module_attrs)*
        #module_vis mod #mod_ident {
            #(#module_items)*
            #[doc = #context_doc]
            pub type ProgramContext = ::arena0::Context<#shared_ty, #local_ty>;
            #[doc = #program_doc]
            pub struct #program_ident;
            #expanded_impl
            #query_impl
        }
        #module_vis use #mod_ident::*;
    })
}

fn module_query_impl(items: &[Item], program_ident: &Ident) -> TokenStream2 {
    let query_ty = if module_has_item_type(items, "Query") {
        quote! { Query }
    } else {
        quote! { () }
    };
    let query_method = match module_fn_typed_arg_count(items, "on_query") {
        // A three-argument callback can inspect the committed participant set.
        Some(3) => quote! {
            fn query(
                shared: &Self::Shared,
                ensemble: &::arena0::Ensemble,
                query: Self::Query,
            ) -> <Self::Query as ::arena0::Arena0Query>::Response {
                self::on_query(shared, ensemble, query)
            }
        },
        // Keep the compact two-argument module callback for projections that
        // only need state. The trait surface remains explicit about the
        // session ensemble so callers that need it do not reconstruct it.
        Some(2) => quote! {
            fn query(
                shared: &Self::Shared,
                _ensemble: &::arena0::Ensemble,
                query: Self::Query,
            ) -> <Self::Query as ::arena0::Arena0Query>::Response {
                self::on_query(shared, query)
            }
        },
        Some(_) | None => quote! {
            fn query(
                _shared: &Self::Shared,
                _ensemble: &::arena0::Ensemble,
                _query: Self::Query,
            ) -> <Self::Query as ::arena0::Arena0Query>::Response {}
        },
    };

    quote! {
        impl ::arena0::ProgramQuery for #program_ident {
            type Query = #query_ty;

            #query_method
        }
    }
}

fn module_assoc_types(
    shared_ty: &Type,
    local_ty: &Type,
    message_ty: &Type,
    callout_ty: &Type,
    input_ty: &Type,
    params_ty: &Type,
    outcome_ty: &Type,
) -> Vec<TokenStream2> {
    vec![
        quote! { type Shared = #shared_ty; },
        quote! { type Local = #local_ty; },
        quote! { type Phase = <#shared_ty as ::arena0::PhasedSharedState>::Phase; },
        quote! { type Message = #message_ty; },
        quote! { type Callout = #callout_ty; },
        quote! { type Input = #input_ty; },
        quote! { type Params = #params_ty; },
        quote! { type Outcome = #outcome_ty; },
    ]
}

fn module_handler_methods(items: &[Item]) -> Vec<TokenStream2> {
    let mut methods = vec![
        quote! {
            #[doc(hidden)]
            fn __phase(shared: &Self::Shared) -> ::core::option::Option<Self::Phase> {
                ::core::option::Option::Some(<Self::Shared as ::arena0::PhasedSharedState>::phase(shared))
            }
        },
        quote! {
            #[doc(hidden)]
            fn __set_phase(shared: &mut Self::Shared, phase: Self::Phase) {
                <Self::Shared as ::arena0::PhasedSharedState>::__set_phase(shared, phase);
            }
        },
        quote! {
            #[doc(hidden)]
            fn __phase_decls() -> &'static [::arena0::PhaseDecl] {
                <<Self::Shared as ::arena0::PhasedSharedState>::Phase as ::arena0::Arena0Phase>::DECLS
            }
        },
    ];
    if module_has_fn(items, "initialize") {
        methods.push(quote! {
            fn initialize(
                shared: &mut Self::Shared,
                params: Self::Params,
            ) -> Result<(), ::arena0::ProgramFault> {
                self::initialize(shared, params)
            }
        });
    }
    if module_has_fn(items, "outcome") {
        methods.push(quote! {
            fn outcome(state: &Self::Shared) -> Self::Outcome {
                self::outcome(state)
            }
        });
    } else {
        methods.push(quote! {
            fn outcome(_shared: &Self::Shared) -> Self::Outcome {}
        });
    }
    if module_has_fn(items, "on_session_started") {
        let arg_count = module_fn_typed_arg_count(items, "on_session_started");
        let uses_ensemble = arg_count == Some(2);
        let ensemble_binding = if uses_ensemble {
            format_ident!("ensemble")
        } else {
            format_ident!("_ensemble")
        };
        let call = if uses_ensemble {
            quote! { self::on_session_started(ctx, #ensemble_binding) }
        } else {
            quote! { self::on_session_started(ctx) }
        };
        methods.push(quote! {
            fn on_session_started(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                #ensemble_binding: &::arena0::Ensemble,
            ) -> Result<::arena0::ProgramTransition<Self>, ::arena0::ProgramFault> {
                #call
            }
        });
    }
    if module_has_fn(items, "on_react") {
        methods.push(quote! {
            fn on_react(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
            ) -> Result<::arena0::ProgramTransition<Self>, ::arena0::ProgramFault> {
                self::on_react(ctx)
            }
        });
    }
    if module_has_fn(items, "writer") {
        methods.push(quote! {
            fn writer(shared: &Self::Shared) -> ::core::option::Option<::arena0::Participant> {
                self::writer(shared)
            }
        });
    } else {
        methods.push(quote! {
            fn writer(_shared: &Self::Shared) -> ::core::option::Option<::arena0::Participant> {
                ::core::option::Option::None
            }
        });
    }
    if module_has_fn(items, "on_message") {
        methods.push(quote! {
            fn on_message(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                from: ::arena0::Participant,
                msg: Self::Message,
            ) -> ::arena0::MessageApply<Self> {
                self::on_message(ctx, from, msg)
            }
        });
    }
    if module_has_fn(items, "on_input") {
        methods.push(quote! {
            fn on_input(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                input: Self::Input,
            ) -> ::arena0::anyhow::Result<::arena0::ProgramTransition<Self>> {
                self::on_input(ctx, input)
            }
        });
    }
    if let Some(timer_ty) = module_typed_timer_arg(items) {
        methods.push(quote! {
            fn on_timer(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                timer: ::arena0::TimerPayload,
            ) -> Result<::arena0::ProgramTransition<Self>, ::arena0::ProgramFault> {
                let timer: #timer_ty = ::arena0::decode_timer_payload(timer)?;
                self::on_timer(ctx, timer)
            }
        });
    } else if module_has_fn(items, "on_timer") {
        methods.push(quote! {
            fn on_timer(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                _timer: ::arena0::TimerPayload,
            ) -> Result<::arena0::ProgramTransition<Self>, ::arena0::ProgramFault> {
                self::on_timer(ctx)
            }
        });
    }
    methods
}

fn module_view_impl(items: &[Item], program_ident: &Ident, shared_ty: &Type) -> TokenStream2 {
    let view_method = if module_has_fn(items, "view") {
        quote! {
            fn view(
                shared: &Self::Shared,
                ensemble: &::arena0::Ensemble,
                viewport: &::arena0::Viewport,
            ) -> ::arena0::View {
                self::view(shared, ensemble, viewport)
            }
        }
    } else {
        default_view_method(program_ident, shared_ty)
    };
    quote! {
        impl ::arena0::ProgramView for #program_ident {
            #view_method
        }
    }
}

fn module_typed_timer_arg(items: &[Item]) -> Option<Type> {
    let function = items.iter().find_map(|item| match item {
        Item::Fn(function) if function.sig.ident == "on_timer" => Some(function),
        _ => None,
    })?;
    let mut typed_inputs = function.sig.inputs.iter().filter_map(|input| match input {
        syn::FnArg::Typed(input) => Some(input),
        syn::FnArg::Receiver(_) => None,
    });
    let _ctx = typed_inputs.next()?;
    let timer = typed_inputs.next()?;
    if typed_inputs.next().is_some() {
        return None;
    }
    Some((*timer.ty).clone())
}

fn module_has_item_type(items: &[Item], name: &str) -> bool {
    items.iter().any(|item| match item {
        Item::Struct(item) => item.ident == name,
        Item::Enum(item) => item.ident == name,
        Item::Type(item) => item.ident == name,
        _ => false,
    })
}

pub(super) fn module_has_fn(items: &[Item], name: &str) -> bool {
    items
        .iter()
        .any(|item| matches!(item, Item::Fn(item) if item.sig.ident == name))
}

fn module_fn_typed_arg_count(items: &[Item], name: &str) -> Option<usize> {
    items.iter().find_map(|item| match item {
        Item::Fn(function) if function.sig.ident == name => Some(
            function
                .sig
                .inputs
                .iter()
                .filter(|input| matches!(input, syn::FnArg::Typed(_)))
                .count(),
        ),
        _ => None,
    })
}

fn rewrite_bare_context_in_module(items: &mut [Item], shared_ty: &Type, local_ty: &Type) {
    for item in items {
        if let Item::Fn(function) = item {
            rewrite_bare_context_in_fn(function, shared_ty, local_ty);
        }
    }
}

fn rewrite_bare_context_in_fn(function: &mut ItemFn, shared_ty: &Type, local_ty: &Type) {
    for arg in &mut function.sig.inputs {
        if let syn::FnArg::Typed(pat_type) = arg {
            rewrite_context_type(&mut pat_type.ty, shared_ty, local_ty);
        }
    }
}

struct ProgramTypes {
    shared: Type,
    local: Type,
    callout: Type,
    message: Type,
    params: Type,
    outcome: Type,
    query: Type,
}

/// Assemble the generated `Program` implementation and resident ABI exports.
///
/// Module shells use the shared resident ABI code generation; this helper is
/// intentionally private to the shell expansion.
#[allow(clippy::too_many_lines)]
fn expand_arena0_program_with_inferred(
    args: Arena0ProgramArgs,
    program_ident: &Ident,
    program_impl: TokenStream2,
    view_impl: TokenStream2,
    types: ProgramTypes,
    extra_inferred_effect_capabilities: Vec<InferredEffectCapability>,
) -> Result<TokenStream2> {
    let ProgramTypes {
        shared: shared_ty,
        local: local_ty,
        callout: callout_ty,
        message: message_ty,
        params: params_ty,
        outcome: outcome_ty,
        query: query_ty,
    } = types;
    let program_ty: Box<Type> = Box::new(syn::parse_quote!(#program_ident));
    let name = &args.name;
    let version = &args.version;
    let description = &args.description;
    let display_name = args.display_name.as_ref().unwrap_or(name);
    let participants = args.participants.to_tokens();
    let capabilities = &args.capabilities;
    let inferred_effect_capabilities = if args.capabilities_auto {
        extra_inferred_effect_capabilities
    } else {
        Vec::new()
    };
    let inferred_effect_capability_tokens: Vec<_> = inferred_effect_capabilities
        .iter()
        .map(|capability| capability.capability.clone())
        .collect();

    Ok(guest_abi(GuestAbi {
        program_impl,
        view_impl,
        program_ty,
        shared_ty,
        local_ty,
        callout_ty,
        message_ty,
        params_ty,
        outcome_ty,
        query_ty,
        name: name.clone(),
        version: version.clone(),
        description: description.clone(),
        display_name: display_name.clone(),
        participants,
        capabilities: capabilities.clone(),
        inferred_effect_capability_tokens,
    }))
}

fn default_view_method(program_ident: &Ident, shared_ty: &Type) -> TokenStream2 {
    quote! {
        fn view(
            shared: &#shared_ty,
            _ensemble: &::arena0::Ensemble,
            _viewport: &::arena0::Viewport,
        ) -> ::arena0::View {
            let mut view = ::arena0::View::new()
                .state(::std::format!("{:#?}", shared));
            if !<#program_ident as ::arena0::Program>::__phase_decls().is_empty() {
                if let ::core::option::Option::Some(phase) =
                    <#program_ident as ::arena0::Program>::__phase(shared)
                {
                    let phase_name =
                        <<#program_ident as ::arena0::Program>::Phase as ::arena0::Arena0Phase>::as_str(phase);
                    view = view.status_bar(phase_name);
                }
            }
            view
        }
    }
}
