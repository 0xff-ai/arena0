//! Inference of effect capabilities from handler bodies.
//!
//! Owns the `syn::visit` pass that scans handler bodies for `ctx.effects()`
//! calls (`send`/`broadcast`, `callout`, `set_timer`, ...) and synchronous
//! `ctx.sign(...)` calls, and derives the `Capability` set declared in program
//! metadata when `capabilities(auto)` is set. This is the capability-inference
//! seam.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use std::collections::HashSet;
use syn::visit::Visit;
use syn::{Expr, Item, ItemFn, Pat, Type};

#[derive(Default)]
struct EffectCapabilityVisitor {
    messaging: bool,
    input: bool,
    timers: bool,
    sign_ed25519: bool,
    sign_bls: bool,
    effect_bindings: HashSet<String>,
    context_bindings: HashSet<String>,
}

#[derive(Clone)]
pub(super) struct InferredEffectCapability {
    pub(super) capability: TokenStream2,
}

impl<'ast> Visit<'ast> for EffectCapabilityVisitor {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        // Parameter and local bindings only apply inside the function that
        // declares them, so each function is analyzed with its own scope.
        let effect_bindings = std::mem::take(&mut self.effect_bindings);
        let context_bindings = std::mem::take(&mut self.context_bindings);
        self.record_context_bindings(&node.sig);
        syn::visit::visit_item_fn(self, node);
        self.effect_bindings = effect_bindings;
        self.context_bindings = context_bindings;
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Pat::Ident(pat) = &node.pat {
            let name = pat.ident.to_string();
            let initializer = node.init.as_ref().map(|init| &*init.expr);
            if initializer.is_some_and(receiver_is_effects_call) {
                self.effect_bindings.insert(name.clone());
            } else {
                self.effect_bindings.remove(&name);
            }
            // A local bound from an existing `Context` receiver (a reborrow
            // such as `&*ctx`) is itself a receiver; every other binding of the
            // same name shadows the parameter and must stop being treated as
            // one.
            if initializer
                .is_some_and(|expr| receiver_is_context_handle(expr, &self.context_bindings))
            {
                self.context_bindings.insert(name);
            } else {
                self.context_bindings.remove(&name);
            }
        }
        syn::visit::visit_local(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if receiver_is_effect_handle(&node.receiver, &self.effect_bindings) {
            match node.method.to_string().as_str() {
                "send" | "broadcast" => self.messaging = true,
                "callout" => self.input = true,
                "set_timer" => self.timers = true,
                _ => {}
            }
        }
        // Guest signing is a synchronous `Context` call rather than an effect
        // handle, so it is recognized by a `Context` receiver, not by method
        // name alone: a program's own `sign` helper must not grant the import.
        if node.method == "sign"
            && receiver_is_context_handle(&node.receiver, &self.context_bindings)
        {
            self.record_sign_scheme(node.args.first());
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

impl EffectCapabilityVisitor {
    fn record_context_bindings(&mut self, signature: &syn::Signature) {
        for input in &signature.inputs {
            if let syn::FnArg::Typed(argument) = input
                && type_is_context(&argument.ty)
                && let Pat::Ident(pat) = &*argument.pat
            {
                self.context_bindings.insert(pat.ident.to_string());
            }
        }
    }

    // A literal scheme argument narrows the declaration to that scheme. Any
    // other expression (a variable, helper call, or array with a non-literal)
    // could request either scheme, so declaring both keeps the sandbox scheme
    // gate from trapping a legally authored program.
    fn record_sign_scheme(&mut self, scheme: Option<&Expr>) {
        match scheme {
            Some(Expr::Path(path)) => self.record_literal_scheme(path),
            Some(Expr::Array(array)) => {
                for element in &array.elems {
                    match element {
                        Expr::Path(path) => self.record_literal_scheme(path),
                        _ => self.record_all_sign_schemes(),
                    }
                }
            }
            _ => self.record_all_sign_schemes(),
        }
    }

    fn record_literal_scheme(&mut self, path: &syn::ExprPath) {
        match sign_scheme_name(path) {
            Some("Ed25519") => self.sign_ed25519 = true,
            Some("Bls") => self.sign_bls = true,
            _ => self.record_all_sign_schemes(),
        }
    }

    fn record_all_sign_schemes(&mut self) {
        self.sign_ed25519 = true;
        self.sign_bls = true;
    }
}

fn sign_scheme_name(path: &syn::ExprPath) -> Option<&'static str> {
    let ident = path.path.segments.last()?.ident.to_string();
    match ident.as_str() {
        "Ed25519" => Some("Ed25519"),
        "Bls" => Some("Bls"),
        _ => None,
    }
}

fn type_is_context(ty: &Type) -> bool {
    match ty {
        Type::Reference(reference) => type_is_context(&reference.elem),
        Type::Paren(paren) => type_is_context(&paren.elem),
        Type::Group(group) => type_is_context(&group.elem),
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Context"),
        _ => false,
    }
}

fn receiver_is_effects_call(expr: &Expr) -> bool {
    matches!(expr, Expr::MethodCall(method) if method.method == "effects")
}

fn receiver_is_effect_handle(expr: &Expr, effect_bindings: &HashSet<String>) -> bool {
    match expr {
        Expr::MethodCall(method) => method.method == "effects",
        Expr::Path(path) if path.path.segments.len() == 1 => {
            effect_bindings.contains(&path.path.segments[0].ident.to_string())
        }
        Expr::Reference(reference) => receiver_is_effect_handle(&reference.expr, effect_bindings),
        Expr::Paren(paren) => receiver_is_effect_handle(&paren.expr, effect_bindings),
        _ => false,
    }
}

fn receiver_is_context_handle(expr: &Expr, context_bindings: &HashSet<String>) -> bool {
    match expr {
        Expr::Path(path) => {
            path.path.segments.len() == 1
                && context_bindings.contains(&path.path.segments[0].ident.to_string())
        }
        Expr::Reference(reference) => receiver_is_context_handle(&reference.expr, context_bindings),
        Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Deref(_)) => {
            receiver_is_context_handle(&unary.expr, context_bindings)
        }
        Expr::Paren(paren) => receiver_is_context_handle(&paren.expr, context_bindings),
        Expr::Group(group) => receiver_is_context_handle(&group.expr, context_bindings),
        _ => false,
    }
}

pub(super) fn infer_effect_capabilities_from_module(
    items: &[Item],
) -> Vec<InferredEffectCapability> {
    let mut visitor = EffectCapabilityVisitor::default();
    for item in items {
        if let Item::Fn(function) = item {
            visitor.visit_item_fn(function);
        }
    }
    inferred_effect_capabilities(visitor)
}

fn inferred_effect_capabilities(visitor: EffectCapabilityVisitor) -> Vec<InferredEffectCapability> {
    let mut capabilities = Vec::new();
    if visitor.messaging {
        capabilities.push(InferredEffectCapability {
            capability: quote! { ::arena0::Capability::Messaging },
        });
    }
    if visitor.input {
        capabilities.push(InferredEffectCapability {
            capability: quote! { ::arena0::Capability::Input },
        });
    }
    if visitor.timers {
        capabilities.push(InferredEffectCapability {
            capability: quote! { ::arena0::Capability::Timers },
        });
    }
    if visitor.sign_ed25519 || visitor.sign_bls {
        let mut schemes = Vec::new();
        if visitor.sign_ed25519 {
            schemes.push(quote! { ::arena0::SignScheme::Ed25519 });
        }
        if visitor.sign_bls {
            schemes.push(quote! { ::arena0::SignScheme::Bls });
        }
        capabilities.push(InferredEffectCapability {
            capability: quote! {
                ::arena0::Capability::Sign { schemes: ::std::vec![#(#schemes),*] }
            },
        });
    }
    capabilities
}
