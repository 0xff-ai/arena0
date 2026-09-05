//! Inference of effect capabilities from handler bodies.
//!
//! Owns the `syn::visit` pass that scans handler bodies for `ctx.effects()`
//! calls (`send`/`broadcast`, `callout`, `set_timer`, `sign`, ...)
//! and derives the `Capability` set declared in program metadata when
//! `capabilities(auto)` is set. This is the capability-inference seam.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use std::collections::HashSet;
use syn::visit::Visit;
use syn::{Expr, Item, ItemImpl, Pat};

#[derive(Default)]
struct EffectCapabilityVisitor {
    messaging: bool,
    input: bool,
    timers: bool,
    sign_ed25519: bool,
    effect_bindings: HashSet<String>,
}

#[derive(Clone)]
pub(super) struct InferredEffectCapability {
    pub(super) capability: TokenStream2,
}

impl<'ast> Visit<'ast> for EffectCapabilityVisitor {
    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init
            && receiver_is_effects_call(&init.expr)
            && let Pat::Ident(pat) = &node.pat
        {
            self.effect_bindings.insert(pat.ident.to_string());
        }
        syn::visit::visit_local(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if receiver_is_effect_handle(&node.receiver, &self.effect_bindings) {
            match node.method.to_string().as_str() {
                "send" | "broadcast" => self.messaging = true,
                "callout" | "callout_typed" => self.input = true,
                "set_timer" => self.timers = true,
                "sign" => self.record_sign_scheme(node.args.first()),
                _ => {}
            }
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

impl EffectCapabilityVisitor {
    // Ed25519 is the only guest-signable scheme; any `sign` call declares it.
    fn record_sign_scheme(&mut self, _scheme: Option<&Expr>) {
        self.sign_ed25519 = true;
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

pub(super) fn infer_effect_capabilities(item: &ItemImpl) -> Vec<InferredEffectCapability> {
    let mut visitor = EffectCapabilityVisitor::default();
    visitor.visit_item_impl(item);
    inferred_effect_capabilities(visitor)
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
    if visitor.sign_ed25519 {
        capabilities.push(InferredEffectCapability {
            capability: quote! { ::arena0::Capability::Sign { schemes: ::std::vec![::arena0::SignScheme::Ed25519] } },
        });
    }
    capabilities
}
