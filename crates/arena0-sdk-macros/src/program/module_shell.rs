//! Expansion of the inline-module (`mod` shell) form of `#[arena0::program]`.
//!
//! Owns `expand_arena0_program_module` and its supporting codegen:
//! generated `Program`/`ProgramQuery`/`ProgramView` impl assembly, associated-
//! type and handler-method synthesis, and the module-item query/view and
//! bare-`Context` rewrite helpers. This is the module-shell codegen seam.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Error, Ident, Item, ItemFn, ItemImpl, ItemMod, Result, Type, spanned::Spanned};

use super::args::Arena0ProgramArgs;
use super::capabilities::infer_effect_capabilities_from_module;
use super::continuations::{
    inject_module_continuation_items, inject_module_local_wrapper, lower_async_module_handlers,
};
use super::host_async::reject_disallowed_host_async_in_module;
use super::trait_form::expand_arena0_program_with_inferred;
use super::{rewrite_context_type, rewrite_shared_context_name, to_pascal_case};

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

    reject_disallowed_host_async_in_module(items)?;

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

    let continuations = lower_async_module_handlers(items, &program_ident)?;
    if module_has_fn(items, "on_message") && !module_has_fn(items, "writer") {
        return Err(Error::new(
            module.span(),
            "arena0::program modules with on_message must define writer(shared) -> Option<Participant>",
        ));
    }
    let local_ty: Type = if continuations.is_empty() {
        user_local_ty.clone()
    } else {
        syn::parse_quote!(__Arena0Local)
    };
    rewrite_bare_context_in_module(items, &shared_ty, &local_ty);
    if !continuations.is_empty() {
        inject_module_local_wrapper(items, &user_local_ty);
        inject_module_continuation_items(
            items,
            &program_ident,
            &shared_ty,
            &local_ty,
            continuations,
        );
    }

    let assoc_types = module_assoc_types(items, &shared_ty, &local_ty);
    let methods = module_handler_methods(items);
    let impl_item: ItemImpl = syn::parse_quote! {
        impl ::arena0::Program for #program_ident {
            #(#assoc_types)*
            #(#methods)*
        }
    };
    let query_impl = module_query_impl(items, &program_ident);
    let inferred_effect_capabilities = if args.capabilities_auto {
        infer_effect_capabilities_from_module(items)
    } else {
        Vec::new()
    };
    let expanded_impl =
        expand_arena0_program_with_inferred(args, impl_item, inferred_effect_capabilities, false)?;
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
    let query_method = if module_has_fn(items, "on_query") {
        quote! {
            fn query(
                ctx: &::arena0::SharedContext<Self::Shared>,
                query: Self::Query,
            ) -> <Self::Query as ::arena0::Arena0Query>::Response {
                self::on_query(ctx, query)
            }
        }
    } else {
        quote! {
            fn query(
                _ctx: &::arena0::SharedContext<Self::Shared>,
                _query: Self::Query,
            ) -> <Self::Query as ::arena0::Arena0Query>::Response {}
        }
    };

    quote! {
        impl ::arena0::ProgramQuery for #program_ident {
            type Query = #query_ty;

            #query_method
        }
    }
}

fn module_assoc_types(items: &[Item], shared_ty: &Type, local_ty: &Type) -> Vec<TokenStream2> {
    let mut assoc_types = vec![
        quote! { type Shared = #shared_ty; },
        quote! { type Local = #local_ty; },
        quote! { type Phase = <#shared_ty as ::arena0::PhasedSharedState>::Phase; },
    ];
    if module_has_item_type(items, "Message") {
        assoc_types.push(quote! { type Message = Message; });
    }
    if module_has_item_type(items, "Callout") {
        assoc_types.push(quote! { type Callout = Callout; });
    }
    if module_has_item_type(items, "Input") {
        assoc_types.push(quote! { type Input = Input; });
    }
    if module_has_item_type(items, "Params") {
        assoc_types.push(quote! { type Params = Params; });
    }
    if module_has_item_type(items, "Outcome") {
        assoc_types.push(quote! { type Outcome = Outcome; });
    }
    assoc_types
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
                ctx: &mut ::arena0::SharedContext<Self::Shared>,
                params: Self::Params,
            ) -> Result<(), ::arena0::ProgramFault> {
                self::initialize(ctx, params)
            }
        });
    }
    if module_has_fn(items, "outcome") {
        methods.push(quote! {
            fn outcome(state: &Self::Shared) -> Self::Outcome {
                self::outcome(state)
            }
        });
    }
    if module_has_fn(items, "view") {
        methods.push(quote! {
            fn view(
                ctx: &::arena0::SharedContext<Self::Shared>,
                viewport: &::arena0::Viewport,
            ) -> ::arena0::View {
                self::view(ctx, viewport)
            }
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
                ctx: &mut ::arena0::SharedContext<Self::Shared>,
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
            ) -> Result<(), ::arena0::ProgramFault> {
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
                ctx: &mut ::arena0::SharedContext<Self::Shared>,
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
            ) -> Result<(), ::arena0::InputFault> {
                self::on_input(ctx, input)
            }
        });
    }
    if module_has_fn(items, "__arena0_on_signed") {
        methods.push(quote! {
            #[doc(hidden)]
            fn __arena0_on_signed(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                signature: ::std::vec::Vec<u8>,
            ) -> Result<(), ::arena0::ProgramFault> {
                self::__arena0_on_signed(ctx, signature)
            }
        });
    }
    if module_has_fn(items, "__arena0_restore_continuation") {
        methods.push(quote! {
            #[doc(hidden)]
            fn __arena0_restore_continuation(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                tag: u32,
            ) {
                self::__arena0_restore_continuation(ctx, tag)
            }
        });
    }
    if let Some(timer_ty) = module_typed_timer_arg(items) {
        methods.push(quote! {
            #[doc(hidden)]
            fn __arena0_on_typed_timer(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
                timer: ::arena0::TimerPayload,
            ) -> Result<(), ::arena0::ProgramFault> {
                let timer: #timer_ty = ::arena0::decode_timer_payload(timer)?;
                self::on_timer(ctx, timer)
            }
        });
    } else if module_has_fn(items, "on_timer") {
        methods.push(quote! {
            fn on_timer(
                ctx: &mut ::arena0::Context<Self::Shared, Self::Local>,
            ) -> Result<(), ::arena0::ProgramFault> {
                self::on_timer(ctx)
            }
        });
    }
    methods
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
    let shared_handler = matches!(
        function.sig.ident.to_string().as_str(),
        "initialize" | "on_session_started" | "on_message" | "on_query" | "view"
    );
    for arg in &mut function.sig.inputs {
        if let syn::FnArg::Typed(pat_type) = arg {
            if shared_handler {
                rewrite_shared_context_name(&mut pat_type.ty);
            }
            rewrite_context_type(&mut pat_type.ty, shared_ty, local_ty);
        }
    }
}
