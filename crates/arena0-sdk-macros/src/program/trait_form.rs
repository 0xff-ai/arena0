//! Expansion of the trait-form (`impl Program`) `#[arena0::program]`.
//!
//! This module owns trait parsing, associated-type defaulting, handler
//! extraction, and rejection of async trait-form handlers. The fresh-instance
//! Wasm ABI emission lives in the sibling `fresh_abi` module.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::visit::Visit;
use syn::{Error, ImplItem, ItemImpl, Result, Type, spanned::Spanned};

use super::args::Arena0ProgramArgs;
use super::capabilities::{InferredEffectCapability, infer_effect_capabilities};
use super::fresh_abi::{FreshGuestAbi, fresh_guest_abi};
use super::{rewrite_context_type, rewrite_shared_context_name};

#[allow(clippy::too_many_lines)]
pub(crate) fn expand_arena0_program(
    args: Arena0ProgramArgs,
    item: ItemImpl,
) -> Result<TokenStream2> {
    expand_arena0_program_with_inferred(args, item, Vec::new(), true)
}

#[allow(clippy::too_many_lines)]
pub(super) fn expand_arena0_program_with_inferred(
    args: Arena0ProgramArgs,
    item: ItemImpl,
    extra_inferred_effect_capabilities: Vec<InferredEffectCapability>,
    emit_query_impl: bool,
) -> Result<TokenStream2> {
    let program_ty = item.self_ty.clone();

    if item.trait_.is_none() {
        return Err(Error::new(
            item.span(),
            "arena0::program can only annotate an impl Program block",
        ));
    }
    reject_async_methods(&item)?;

    let name = &args.name;
    let version = &args.version;
    let description = &args.description;
    let display_name = args.display_name.as_ref().unwrap_or(name);
    let participants = args.participants.to_tokens();
    let capabilities = &args.capabilities;
    let inferred_effect_capabilities = if args.capabilities_auto {
        let mut inferred = infer_effect_capabilities(&item);
        inferred.extend(extra_inferred_effect_capabilities);
        inferred
    } else {
        Vec::new()
    };
    let inferred_effect_capability_tokens: Vec<_> = inferred_effect_capabilities
        .iter()
        .map(|capability| capability.capability.clone())
        .collect();

    let shared_ty = extract_assoc_type(&item, "Shared")?;
    let has_local = extract_assoc_type(&item, "Local").is_ok();
    let has_phase = extract_assoc_type(&item, "Phase").is_ok();
    let has_message = extract_assoc_type(&item, "Message").is_ok();
    let has_callout = extract_assoc_type(&item, "Callout").is_ok();
    let has_input = extract_assoc_type(&item, "Input").is_ok();
    let has_params = extract_assoc_type(&item, "Params").is_ok();
    let has_outcome = extract_assoc_type(&item, "Outcome").is_ok();
    let has_outcome_fn = item.items.iter().any(
        |impl_item| matches!(impl_item, ImplItem::Fn(method) if method.sig.ident == "outcome"),
    );

    let local_ty: Type = if has_local {
        extract_assoc_type(&item, "Local").unwrap()
    } else {
        syn::parse_quote!(())
    };
    let message_ty: Type = if has_message {
        extract_assoc_type(&item, "Message").unwrap()
    } else {
        syn::parse_quote!(Vec<u8>)
    };
    let callout_ty: Type = if has_callout {
        extract_assoc_type(&item, "Callout").unwrap()
    } else {
        syn::parse_quote!(())
    };
    if has_input {
        let _ = extract_assoc_type(&item, "Input")?;
    }
    let params_ty: Type = if has_params {
        extract_assoc_type(&item, "Params").unwrap()
    } else {
        syn::parse_quote!(())
    };
    let outcome_ty: Type = if has_outcome {
        extract_assoc_type(&item, "Outcome").unwrap()
    } else {
        syn::parse_quote!(())
    };
    let mut item = item;

    // Shared handlers are rewritten to the shared-only context before the
    // trait implementation is emitted; local handlers retain the local
    // context with identity, local state, and effects.
    rewrite_bare_context(&mut item, &shared_ty, &local_ty);
    let (query_impl, query_ty) = if emit_query_impl {
        extract_program_query_impl(&mut item, &program_ty, &shared_ty, &local_ty)
    } else {
        let query_ty: Type = syn::parse_quote!(<#program_ty as ::arena0::ProgramQuery>::Query);
        (quote! {}, query_ty)
    };
    let (view_impl, _has_view) =
        extract_program_view_impl(&mut item, &program_ty, &shared_ty, &local_ty);

    if !has_local {
        item.items.push(syn::parse_quote! { type Local = (); });
    }
    if !has_phase {
        item.items.push(
            syn::parse_quote! { type Phase = <#shared_ty as ::arena0::PhasedSharedState>::Phase; },
        );
        item.items.push(syn::parse_quote! {
            #[doc(hidden)]
            fn __phase(shared: &Self::Shared) -> ::core::option::Option<Self::Phase> {
                ::core::option::Option::Some(<#shared_ty as ::arena0::PhasedSharedState>::phase(shared))
            }
        });
        item.items.push(syn::parse_quote! {
            #[doc(hidden)]
            fn __set_phase(shared: &mut Self::Shared, phase: Self::Phase) {
                <#shared_ty as ::arena0::PhasedSharedState>::__set_phase(shared, phase);
            }
        });
        item.items.push(syn::parse_quote! {
            #[doc(hidden)]
            fn __phase_decls() -> &'static [::arena0::PhaseDecl] {
                <<#shared_ty as ::arena0::PhasedSharedState>::Phase as ::arena0::Arena0Phase>::DECLS
            }
        });
    }
    if !has_message {
        item.items
            .push(syn::parse_quote! { type Message = Vec<u8>; });
    }
    if !has_callout {
        item.items.push(syn::parse_quote! { type Callout = (); });
    }
    if !has_input {
        item.items.push(
            syn::parse_quote! { type Input = <#callout_ty as ::arena0::Arena0Callout>::Response; },
        );
    }
    if !has_params {
        item.items.push(syn::parse_quote! { type Params = (); });
    }
    if !has_outcome {
        item.items.push(syn::parse_quote! { type Outcome = (); });
        // No declared outcome type: the terminal receipt is the unit projection.
        if !has_outcome_fn {
            item.items.push(syn::parse_quote! {
                fn outcome(_shared: &Self::Shared) -> Self::Outcome {}
            });
        }
    }

    Ok(fresh_guest_abi(FreshGuestAbi {
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
        name: name.clone(),
        version: version.clone(),
        description: description.clone(),
        display_name: display_name.clone(),
        participants,
        capabilities: capabilities.clone(),
        inferred_effect_capability_tokens,
    }))
}

fn extract_program_query_impl(
    item: &mut ItemImpl,
    program_ty: &Type,
    shared_ty: &Type,
    _local_ty: &Type,
) -> (TokenStream2, Type) {
    let mut retained = Vec::new();
    let mut query_ty: Option<Type> = None;
    let mut query_method = None;

    for impl_item in std::mem::take(&mut item.items) {
        match impl_item {
            ImplItem::Type(ty) if ty.ident == "Query" => {
                query_ty = Some(ty.ty);
            }
            ImplItem::Fn(mut method) if method.sig.ident == "on_query" => {
                method.sig.ident = format_ident!("query");
                query_method = Some(method);
            }
            other => retained.push(other),
        }
    }

    item.items = retained;
    let query_ty = query_ty.unwrap_or_else(|| syn::parse_quote!(()));
    let query_method = query_method.map_or_else(
        || {
            quote! {
                fn query(
                    _ctx: &::arena0::SharedContext<#shared_ty>,
                    _query: Self::Query,
                ) -> <Self::Query as ::arena0::Arena0Query>::Response {}
            }
        },
        |method| {
            quote! {
                #method
            }
        },
    );

    (
        quote! {
            impl ::arena0::ProgramQuery for #program_ty {
                type Query = #query_ty;

                #query_method
            }
        },
        query_ty,
    )
}

fn extract_program_view_impl(
    item: &mut ItemImpl,
    program_ty: &Type,
    shared_ty: &Type,
    local_ty: &Type,
) -> (TokenStream2, bool) {
    let mut retained = Vec::new();
    let mut view_method = None;

    for impl_item in std::mem::take(&mut item.items) {
        match impl_item {
            ImplItem::Fn(method) if method.sig.ident == "view" => {
                view_method = Some(method);
            }
            other => retained.push(other),
        }
    }

    item.items = retained;
    let has_view = view_method.is_some();
    let view_method = view_method.map_or_else(
        || default_view_method(program_ty, shared_ty, local_ty),
        |method| {
            quote! {
                #method
            }
        },
    );

    (
        quote! {
            impl ::arena0::ProgramView for #program_ty {
                #view_method
            }
        },
        has_view,
    )
}

fn default_view_method(program_ty: &Type, shared_ty: &Type, _local_ty: &Type) -> TokenStream2 {
    quote! {
        fn view(
            ctx: &::arena0::SharedContext<#shared_ty>,
            _viewport: &::arena0::Viewport,
        ) -> ::arena0::View {
            let mut view = ::arena0::View::new()
                .state(::std::format!("{:#?}", ctx.shared()));
            if !<#program_ty as ::arena0::Program>::__phase_decls().is_empty() {
                if let ::core::option::Option::Some(phase) =
                    <#program_ty as ::arena0::Program>::__phase(ctx.shared())
                {
                    let phase_name =
                        <<#program_ty as ::arena0::Program>::Phase as ::arena0::Arena0Phase>::as_str(phase);
                    view = view.status_bar(phase_name);
                }
            }
            view
        }
    }
}

/// Rewrite bare `Context` (no generic args) in method signatures to
/// `Context<Shared, Local>`. This lets programs write the shorter form
/// while the macro fills in what it already knows from associated types.
fn rewrite_bare_context(item: &mut ItemImpl, shared_ty: &Type, local_ty: &Type) {
    for impl_item in &mut item.items {
        if let ImplItem::Fn(method) = impl_item {
            let shared_handler = matches!(
                method.sig.ident.to_string().as_str(),
                "initialize" | "on_session_started" | "on_message"
            );
            for arg in &mut method.sig.inputs {
                if let syn::FnArg::Typed(pat_type) = arg {
                    if shared_handler {
                        rewrite_shared_context_name(&mut pat_type.ty);
                    }
                    rewrite_context_type(&mut pat_type.ty, shared_ty, local_ty);
                }
            }
        }
    }
}

fn extract_assoc_type(item: &ItemImpl, name: &str) -> Result<Type> {
    for it in &item.items {
        if let ImplItem::Type(ty) = it
            && ty.ident == name
        {
            return Ok(ty.ty.clone());
        }
    }
    Err(Error::new(
        item.span(),
        format!("missing associated type `{name}`"),
    ))
}

fn reject_async_methods(item: &ItemImpl) -> Result<()> {
    for impl_item in &item.items {
        if let ImplItem::Fn(method) = impl_item
            && let Some(asyncness) = method.sig.asyncness
        {
            return Err(Error::new(
                asyncness.span(),
                "async #[arena0::program] trait-form handlers are not supported; use synchronous handlers or a generated module shell so arena0-owned awaits can be lowered into traced continuations",
            ));
        }
        if let ImplItem::Fn(method) = impl_item {
            let mut visitor = TraitFormAsyncVisitor { error: None };
            visitor.visit_block(&method.block);
            if let Some(error) = visitor.error {
                return Err(error);
            }
        }
    }
    Ok(())
}

pub(super) struct TraitFormAsyncVisitor {
    pub(super) error: Option<Error>,
}

impl TraitFormAsyncVisitor {
    fn reject(&mut self, span: proc_macro2::Span, message: &'static str) {
        if self.error.is_none() {
            self.error = Some(Error::new(span, message));
        }
    }
}

impl<'ast> Visit<'ast> for TraitFormAsyncVisitor {
    fn visit_expr_async(&mut self, node: &'ast syn::ExprAsync) {
        self.reject(
            node.async_token.span(),
            "async blocks are not supported inside #[arena0::program] trait-form handlers; use .dispatch() or move arena-owned awaits into a generated module shell",
        );
    }

    fn visit_expr_await(&mut self, node: &'ast syn::ExprAwait) {
        self.reject(
            node.await_token.span(),
            "await is not supported inside #[arena0::program] trait-form handlers; use .dispatch() or move arena-owned awaits into a generated module shell",
        );
    }
}
