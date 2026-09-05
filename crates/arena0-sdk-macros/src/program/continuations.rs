//! Lowering of `async` module-shell handlers into traced continuations.
//!
//! Owns the transform that splits an `async` handler at each awaited arena
//! effect (callout or sign) into resume functions plus a `__Arena0Continuation`
//! state machine, validates that bindings survive across an await, and injects
//! the continuation dispatch and local-wrapper items. This is the continuation
//! lowering seam.

use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::visit::Visit;
use syn::{Error, Expr, Ident, Item, ItemFn, Pat, Result, Stmt, Token, Type, spanned::Spanned};

use super::module_shell::module_has_fn;
use super::to_pascal_case;

pub(super) struct ModuleContinuation {
    variant_ident: Ident,
    resume_fn_ident: Ident,
    resume_fn: ItemFn,
    kind: ModuleContinuationKind,
}

#[derive(Clone)]
enum ModuleContinuationKind {
    Callout { request_ty: Box<Type> },
    Sign,
}

pub(super) fn lower_async_module_handlers(
    items: &mut [Item],
    program_ident: &Ident,
) -> Result<Vec<ModuleContinuation>> {
    let mut continuations = Vec::new();
    for item in items.iter_mut() {
        let Item::Fn(function) = item else {
            continue;
        };
        if function.sig.asyncness.is_none() {
            continue;
        }
        if matches!(
            function.sig.ident.to_string().as_str(),
            "initialize" | "on_session_started" | "on_message" | "on_query" | "view"
        ) {
            return Err(Error::new(
                function.sig.ident.span(),
                "async shared handlers are not supported; shared dispatch must be a synchronous function of shared state and its public event",
            ));
        }
        if function.sig.ident == "on_input" {
            return Err(Error::new(
                function.sig.ident.span(),
                "async on_input continuations are not lowered yet; use a synchronous on_input handler",
            ));
        }
        let index = continuations.len();
        continuations.extend(lower_async_module_handler(function, program_ident, index)?);
    }

    if !continuations.is_empty() {
        for item in items.iter_mut() {
            if let Item::Fn(function) = item
                && function.sig.ident == "on_input"
            {
                function.sig.ident = format_ident!("__arena0_user_on_input");
            }
        }
    }

    Ok(continuations)
}

fn lower_async_module_handler(
    function: &mut ItemFn,
    program_ident: &Ident,
    index: usize,
) -> Result<Vec<ModuleContinuation>> {
    let asyncness = function.sig.asyncness.take().expect("checked by caller");
    let handler_name = function.sig.ident.to_string();

    let mut await_stmts = Vec::new();
    for (stmt_index, stmt) in function.block.stmts.iter().enumerate() {
        if let Some(awaited) = awaited_effect_stmt(stmt)? {
            await_stmts.push((stmt_index, awaited));
        } else if let Some(span) = first_await_span(std::slice::from_ref(stmt)) {
            return Err(Error::new(
                span,
                "await is only supported on direct arena effect statements in module shells",
            ));
        }
    }
    if await_stmts.is_empty() {
        return Err(Error::new(
            asyncness.span(),
            "async module-shell handlers require at least one direct awaited arena effect",
        ));
    }
    validate_continuation_captures(function, &await_stmts)?;

    let mut continuations = Vec::new();
    let await_count = await_stmts.len();
    let variant_idents: Vec<_> = (0..await_count)
        .map(|offset| format_ident!("{}Await{}", to_pascal_case(&handler_name), index + offset))
        .collect();

    for await_idx in 0..await_count {
        let stmt_index = await_stmts[await_idx].0;
        let next_stmt_index = await_stmts.get(await_idx + 1).map(|(idx, _)| *idx);
        let AwaitedEffect { pat, kind, .. } = &await_stmts[await_idx].1;
        let resume_fn_ident =
            format_ident!("__arena0_resume_{}_{}", handler_name, index + await_idx);
        let resume_arg_ty = continuation_output_ty(program_ident, kind);
        let resume_fault_ty = continuation_fault_ty(kind);
        let post_stmts = match next_stmt_index {
            Some(next) => function.block.stmts[stmt_index + 1..next].to_vec(),
            None => function.block.stmts[stmt_index + 1..].to_vec(),
        };
        let body =
            if let Some(next_await_idx) = await_stmts.get(await_idx + 1).map(|_| await_idx + 1) {
                let next_variant = &variant_idents[next_await_idx];
                let next_effect_expr = &await_stmts[next_await_idx].1.effect_expr;
                quote! {
                    #(#post_stmts)*
                    let __arena0_continuation = __Arena0Continuation::#next_variant;
                    __arena0_set_continuation(ctx, __arena0_continuation);
                    #next_effect_expr
                        .__continuation_tag(__arena0_continuation.__tag())
                        .dispatch();
                    Ok(())
                }
            } else {
                quote! {
                    #(#post_stmts)*
                }
            };

        let resume_fn: ItemFn = syn::parse_quote! {
            fn #resume_fn_ident(
                ctx: &mut ::arena0::Context<
                    <#program_ident as ::arena0::Program>::Shared,
                    <#program_ident as ::arena0::Program>::Local,
                >,
                #pat: #resume_arg_ty,
            ) -> ::core::result::Result<(), #resume_fault_ty> {
                #body
            }
        };

        continuations.push(ModuleContinuation {
            variant_ident: variant_idents[await_idx].clone(),
            resume_fn_ident: resume_fn.sig.ident.clone(),
            resume_fn,
            kind: kind.clone(),
        });
    }

    let first_stmt_index = await_stmts[0].0;
    let first_variant = &variant_idents[0];
    let first_effect_expr = &await_stmts[0].1.effect_expr;
    let pre_stmts = function.block.stmts[..first_stmt_index].to_vec();
    *function.block = syn::parse_quote!({
        #(#pre_stmts)*
        let __arena0_continuation = __Arena0Continuation::#first_variant;
        __arena0_set_continuation(ctx, __arena0_continuation);
        #first_effect_expr
            .__continuation_tag(__arena0_continuation.__tag())
            .dispatch();
        Ok(())
    });

    Ok(continuations)
}

fn validate_continuation_captures(
    function: &ItemFn,
    await_stmts: &[(usize, AwaitedEffect)],
) -> Result<()> {
    let mut previous_await_bindings = HashSet::new();
    let param_bindings = bindings_in_fn_args(&function.sig.inputs);
    for (await_idx, (stmt_index, awaited)) in await_stmts.iter().enumerate() {
        let next_stmt_index = await_stmts
            .get(await_idx + 1)
            .map(|(idx, _)| *idx)
            .unwrap_or(function.block.stmts.len());
        let mut unavailable = param_bindings.clone();
        for stmt in &function.block.stmts[..*stmt_index] {
            collect_local_bindings(stmt, &mut unavailable);
        }
        unavailable.extend(previous_await_bindings.iter().cloned());
        unavailable.remove("ctx");
        for current in bindings_in_pat(&awaited.pat) {
            unavailable.remove(&current);
        }

        let mut visitor = ContinuationCaptureVisitor {
            unavailable: &unavailable,
            hit: None,
        };
        for stmt in &function.block.stmts[stmt_index + 1..next_stmt_index] {
            visitor.visit_stmt(stmt);
            if let Some((ident, span)) = visitor.hit {
                return Err(Error::new(
                    span,
                    format!(
                        "`{ident}` was declared before an arena await and is not captured across the generated continuation; move needed data into Local or Shared before awaiting, or recompute it after resume"
                    ),
                ));
            }
        }
        previous_await_bindings.extend(bindings_in_pat(&awaited.pat));
    }
    Ok(())
}

fn bindings_in_fn_args(
    inputs: &syn::punctuated::Punctuated<syn::FnArg, Token![,]>,
) -> HashSet<String> {
    let mut bindings = HashSet::new();
    for input in inputs {
        if let syn::FnArg::Typed(pat) = input {
            collect_pat_bindings(&pat.pat, &mut bindings);
        }
    }
    bindings
}

fn collect_local_bindings(stmt: &Stmt, bindings: &mut HashSet<String>) {
    if let Stmt::Local(local) = stmt {
        collect_pat_bindings(&local.pat, bindings);
    }
}

fn bindings_in_pat(pat: &Pat) -> HashSet<String> {
    let mut bindings = HashSet::new();
    collect_pat_bindings(pat, &mut bindings);
    bindings
}

fn collect_pat_bindings(pat: &Pat, bindings: &mut HashSet<String>) {
    match pat {
        Pat::Ident(ident) => {
            let name = ident.ident.to_string();
            if name != "_" && !name.starts_with('_') {
                bindings.insert(name);
            }
            if let Some((_at, subpat)) = &ident.subpat {
                collect_pat_bindings(subpat, bindings);
            }
        }
        Pat::Tuple(tuple) => {
            for elem in &tuple.elems {
                collect_pat_bindings(elem, bindings);
            }
        }
        Pat::TupleStruct(tuple) => {
            for elem in &tuple.elems {
                collect_pat_bindings(elem, bindings);
            }
        }
        Pat::Struct(strukt) => {
            for field in &strukt.fields {
                collect_pat_bindings(&field.pat, bindings);
            }
        }
        Pat::Slice(slice) => {
            for elem in &slice.elems {
                collect_pat_bindings(elem, bindings);
            }
        }
        Pat::Reference(reference) => collect_pat_bindings(&reference.pat, bindings),
        Pat::Or(or) => {
            for case in &or.cases {
                collect_pat_bindings(case, bindings);
            }
        }
        Pat::Type(ty) => collect_pat_bindings(&ty.pat, bindings),
        Pat::Paren(paren) => collect_pat_bindings(&paren.pat, bindings),
        Pat::Rest(_)
        | Pat::Lit(_)
        | Pat::Macro(_)
        | Pat::Path(_)
        | Pat::Range(_)
        | Pat::Verbatim(_)
        | Pat::Wild(_) => {}
        _ => {}
    }
}

struct ContinuationCaptureVisitor<'a> {
    unavailable: &'a HashSet<String>,
    hit: Option<(String, proc_macro2::Span)>,
}

impl<'ast> Visit<'ast> for ContinuationCaptureVisitor<'_> {
    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if self.hit.is_some() {
            return;
        }
        if node.path.segments.len() == 1 {
            let ident = &node.path.segments[0].ident;
            let name = ident.to_string();
            if self.unavailable.contains(&name) {
                self.hit = Some((name, ident.span()));
                return;
            }
        }
        syn::visit::visit_expr_path(self, node);
    }
}

fn continuation_output_ty(program_ident: &Ident, kind: &ModuleContinuationKind) -> Type {
    match kind {
        ModuleContinuationKind::Callout { request_ty } => {
            syn::parse_quote!(<#request_ty as ::arena0::CalloutSpec<#program_ident>>::Output)
        }
        ModuleContinuationKind::Sign => syn::parse_quote!(::std::vec::Vec<u8>),
    }
}

fn continuation_fault_ty(kind: &ModuleContinuationKind) -> Type {
    match kind {
        ModuleContinuationKind::Callout { .. } => syn::parse_quote!(::arena0::InputFault),
        ModuleContinuationKind::Sign => syn::parse_quote!(::arena0::ProgramFault),
    }
}

struct AwaitedEffect {
    pat: Pat,
    effect_expr: Expr,
    kind: ModuleContinuationKind,
}

fn awaited_effect_stmt(stmt: &Stmt) -> Result<Option<AwaitedEffect>> {
    match stmt {
        Stmt::Local(local) => {
            let Some(init) = &local.init else {
                return Ok(None);
            };
            let Some(effect_expr) = try_await_base(&init.expr) else {
                return Ok(None);
            };
            let kind = awaited_effect_kind(effect_expr)?;
            Ok(Some(AwaitedEffect {
                pat: local.pat.clone(),
                effect_expr: effect_expr.clone(),
                kind,
            }))
        }
        Stmt::Expr(expr, _) => {
            let Some(effect_expr) = try_await_base(expr) else {
                return Ok(None);
            };
            let kind = awaited_effect_kind(effect_expr)?;
            Ok(Some(AwaitedEffect {
                pat: syn::parse_quote!(_),
                effect_expr: effect_expr.clone(),
                kind,
            }))
        }
        _ => Ok(None),
    }
}

fn try_await_base(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Try(try_expr) => match try_expr.expr.as_ref() {
            Expr::Await(await_expr) => Some(await_expr.base.as_ref()),
            _ => None,
        },
        Expr::Await(await_expr) => Some(await_expr.base.as_ref()),
        _ => None,
    }
}

fn awaited_effect_kind(expr: &Expr) -> Result<ModuleContinuationKind> {
    if find_method_call(expr, "sign").is_some() {
        return Ok(ModuleContinuationKind::Sign);
    }
    let Some(request_expr) = find_callout_request_expr(expr) else {
        return Err(Error::new(
            expr.span(),
            "await is only supported on direct ctx.effects().callout(...) or ctx.effects().sign(...) builders in module shells",
        ));
    };
    let Expr::Struct(request) = request_expr else {
        return Err(Error::new(
            request_expr.span(),
            "awaited callouts must use a concrete generated callout request struct, such as callouts::Choose { ... }",
        ));
    };
    let path = request.path.clone();
    Ok(ModuleContinuationKind::Callout {
        request_ty: Box::new(syn::parse_quote!(#path)),
    })
}

fn find_callout_request_expr(expr: &Expr) -> Option<&Expr> {
    let Expr::MethodCall(method) = expr else {
        return None;
    };
    if (method.method == "callout" || method.method == "callout_typed") && method.args.len() == 1 {
        return method.args.first();
    }
    find_callout_request_expr(&method.receiver)
}

fn find_method_call<'a>(expr: &'a Expr, name: &str) -> Option<&'a syn::ExprMethodCall> {
    let Expr::MethodCall(method) = expr else {
        return None;
    };
    if method.method == name {
        return Some(method);
    }
    find_method_call(&method.receiver, name)
}

fn first_await_span(stmts: &[Stmt]) -> Option<proc_macro2::Span> {
    let mut visitor = AwaitSpanVisitor { span: None };
    for stmt in stmts {
        visitor.visit_stmt(stmt);
        if visitor.span.is_some() {
            break;
        }
    }
    visitor.span
}

struct AwaitSpanVisitor {
    span: Option<proc_macro2::Span>,
}

impl<'ast> Visit<'ast> for AwaitSpanVisitor {
    fn visit_expr_await(&mut self, node: &'ast syn::ExprAwait) {
        if self.span.is_none() {
            self.span = Some(node.await_token.span());
        }
    }
}

pub(super) fn inject_module_continuation_items(
    items: &mut Vec<Item>,
    program_ident: &Ident,
    shared_ty: &Type,
    local_ty: &Type,
    continuations: Vec<ModuleContinuation>,
) {
    for item in items.iter_mut() {
        if let Item::Fn(function) = item
            && function.sig.ident == "initialize"
        {
            function.sig.ident = format_ident!("__arena0_user_initialize");
        }
    }

    let variants: Vec<_> = continuations
        .iter()
        .map(|continuation| &continuation.variant_ident)
        .collect();
    let tag_arms: Vec<_> = continuations
        .iter()
        .enumerate()
        .map(|(index, continuation)| {
            let variant = &continuation.variant_ident;
            let tag = index as u32;
            quote! { __Arena0Continuation::#variant => #tag }
        })
        .collect();
    let from_tag_arms: Vec<_> = continuations
        .iter()
        .enumerate()
        .map(|(index, continuation)| {
            let variant = &continuation.variant_ident;
            let tag = index as u32;
            quote! { #tag => ::core::option::Option::Some(__Arena0Continuation::#variant) }
        })
        .collect();
    let input_arms: Vec<_> = continuations
        .iter()
        .filter_map(|continuation| match &continuation.kind {
            ModuleContinuationKind::Callout { request_ty } => {
                let variant = &continuation.variant_ident;
                let resume_fn = &continuation.resume_fn_ident;
                Some(quote! {
                    Some(__Arena0Continuation::#variant) => {
                        let __arena0_value =
                            match <#request_ty as ::arena0::CalloutSpec<#program_ident>>::decode(input) {
                                Ok(__value) => __value,
                                Err(__error) => {
                                    __arena0_set_continuation(ctx, __Arena0Continuation::#variant);
                                    return Err(__error);
                                }
                            };
                        self::#resume_fn(ctx, __arena0_value)
                    }
                })
            }
            ModuleContinuationKind::Sign => None,
        })
        .collect();
    let recover_input_arms: Vec<_> = continuations
        .iter()
        .filter_map(|continuation| match &continuation.kind {
            ModuleContinuationKind::Callout { request_ty } => {
                let resume_fn = &continuation.resume_fn_ident;
                Some(quote! {
                    if let Ok(__arena0_value) =
                        <#request_ty as ::arena0::CalloutSpec<#program_ident>>::decode(input.clone())
                    {
                        return self::#resume_fn(ctx, __arena0_value);
                    }
                })
            }
            ModuleContinuationKind::Sign => None,
        })
        .collect();
    let signed_arms: Vec<_> = continuations
        .iter()
        .filter_map(|continuation| match &continuation.kind {
            ModuleContinuationKind::Sign => {
                let variant = &continuation.variant_ident;
                let resume_fn = &continuation.resume_fn_ident;
                Some(quote! {
                    Some(__Arena0Continuation::#variant) => self::#resume_fn(ctx, signature),
                })
            }
            ModuleContinuationKind::Callout { .. } => None,
        })
        .collect();
    let fallback = if module_has_fn(items, "__arena0_user_on_input") {
        quote! {
            None => self::__arena0_user_on_input(ctx, input),
        }
    } else {
        quote! {
            None => {
                #(#recover_input_arms)*
                Ok(())
            },
        }
    };

    for continuation in &continuations {
        items.push(Item::Fn(continuation.resume_fn.clone()));
    }

    items.push(syn::parse_quote! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum __Arena0Continuation {
            #(#variants),*
        }
    });
    items.push(syn::parse_quote! {
        impl __Arena0Continuation {
            fn __tag(self) -> u32 {
                match self {
                    #(#tag_arms),*
                }
            }

            fn __from_tag(tag: u32) -> ::core::option::Option<Self> {
                match tag {
                    #(#from_tag_arms,)*
                    _ => ::core::option::Option::None,
                }
            }
        }
    });
    items.push(syn::parse_quote! {
        fn __arena0_set_continuation(
            ctx: &mut ::arena0::Context<#shared_ty, #local_ty>,
            continuation: __Arena0Continuation,
        ) {
            let __arena0_tag = continuation.__tag();
            ctx.local_mut().__arena0_continuation = ::core::option::Option::Some(__arena0_tag);
        }
    });
    items.push(syn::parse_quote! {
        fn __arena0_take_continuation(
            ctx: &mut ::arena0::Context<#shared_ty, #local_ty>,
        ) -> ::core::option::Option<__Arena0Continuation> {
            let __arena0_tag = ctx.local().__arena0_continuation?;
            ctx.local_mut().__arena0_continuation = ::core::option::Option::None;
            __Arena0Continuation::__from_tag(__arena0_tag)
        }
    });
    items.push(syn::parse_quote! {
        fn __arena0_clear_continuation(
            ctx: &mut ::arena0::Context<#shared_ty, #local_ty>,
        ) {
            ctx.local_mut().__arena0_continuation = ::core::option::Option::None;
        }
    });
    items.push(syn::parse_quote! {
        fn __arena0_restore_continuation(
            ctx: &mut ::arena0::Context<#shared_ty, #local_ty>,
            tag: u32,
        ) {
            if let ::core::option::Option::Some(continuation) = __Arena0Continuation::__from_tag(tag) {
                __arena0_set_continuation(ctx, continuation);
            }
        }
    });
    let initialize_fallback = if module_has_fn(items, "__arena0_user_initialize") {
        quote! {
            self::__arena0_user_initialize(ctx, params)
        }
    } else {
        quote! {
            Ok(())
        }
    };
    items.push(syn::parse_quote! {
        fn initialize(
            ctx: &mut ::arena0::SharedContext<#shared_ty>,
            params: <#program_ident as ::arena0::Program>::Params,
        ) -> ::core::result::Result<(), ::arena0::ProgramFault> {
            let _ = &params;
            #initialize_fallback
        }
    });
    items.push(syn::parse_quote! {
        fn on_input(
            ctx: &mut ::arena0::Context<#shared_ty, #local_ty>,
            input: <#program_ident as ::arena0::Program>::Input,
        ) -> ::core::result::Result<(), ::arena0::InputFault> {
            use ::arena0::anyhow::anyhow;

            match __arena0_take_continuation(ctx) {
                #(#input_arms)*
                #fallback
                Some(__other) => {
                    __arena0_set_continuation(ctx, __other);
                    Err(::arena0::InputFault::Unrecoverable(
                        anyhow!("pending continuation does not accept input"),
                    ))
                }
            }
        }
    });
    items.push(syn::parse_quote! {
        fn __arena0_on_signed(
            ctx: &mut ::arena0::Context<#shared_ty, #local_ty>,
            signature: ::std::vec::Vec<u8>,
        ) -> ::core::result::Result<(), ::arena0::ProgramFault> {
            use ::arena0::anyhow::anyhow;

            match __arena0_take_continuation(ctx) {
                #(#signed_arms)*
                None => Ok(()),
                Some(__other) => {
                    __arena0_set_continuation(ctx, __other);
                    Err(::arena0::ProgramFault(
                        anyhow!("pending continuation does not accept a signature"),
                    ))
                }
            }
        }
    });
}

pub(super) fn inject_module_local_wrapper(items: &mut Vec<Item>, user_local_ty: &Type) {
    items.push(syn::parse_quote! {
        #[doc(hidden)]
        #[derive(Default)]
        pub struct __Arena0Local {
            __arena0_user: #user_local_ty,
            __arena0_continuation: ::core::option::Option<u32>,
        }
    });
    items.push(syn::parse_quote! {
        impl ::core::ops::Deref for __Arena0Local {
            type Target = #user_local_ty;

            fn deref(&self) -> &Self::Target {
                &self.__arena0_user
            }
        }
    });
    items.push(syn::parse_quote! {
        impl ::core::ops::DerefMut for __Arena0Local {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.__arena0_user
            }
        }
    });
}
