//! The `#[arena0::program]` attribute expansion.
//!
//! This module coordinates the two program forms and holds the codegen helpers
//! shared between them. The seams live in submodules:
//!
//! - [`args`]: attribute parsing and program metadata.
//! - [`trait_form`]: `impl Program` expansion and handler extraction.
//! - [`fresh_abi`]: fresh-instance guest ABI export emission.
//! - [`module_shell`]: inline-`mod` shell expansion.
//! - [`continuations`]: lowering of `async` module-shell handlers.
//! - [`host_async`]: rejection of host async/IO in module-shell handlers.
//! - [`capabilities`]: effect-capability inference for `capabilities(auto)`.

use proc_macro2::TokenStream as TokenStream2;
use quote::format_ident;
use syn::{Error, Item, Result, Type, spanned::Spanned};

mod args;
mod capabilities;
mod continuations;
mod fresh_abi;
mod host_async;
mod module_shell;
mod trait_form;

#[cfg(test)]
mod tests;

use crate::util::to_pascal_case;
pub(crate) use args::Arena0ProgramArgs;
use module_shell::expand_arena0_program_module;
use trait_form::expand_arena0_program;

pub(crate) fn expand_arena0_program_item(
    args: Arena0ProgramArgs,
    item: Item,
) -> Result<TokenStream2> {
    match item {
        Item::Impl(item) => expand_arena0_program(args, item),
        Item::Mod(item) => expand_arena0_program_module(args, item),
        other => Err(Error::new(
            other.span(),
            "arena0::program can only annotate an impl Program block or an inline module shell",
        )),
    }
}

/// Recursively rewrite context types to their generic forms inside a type.
fn rewrite_context_type(ty: &mut Type, shared_ty: &Type, local_ty: &Type) {
    match ty {
        Type::Path(type_path) => {
            if let Some(segment) = type_path.path.segments.last_mut()
                && (segment.ident == "Context" || segment.ident == "SharedContext")
                && segment.arguments.is_empty()
            {
                segment.arguments = if segment.ident == "SharedContext" {
                    syn::PathArguments::AngleBracketed(syn::parse_quote!(<#shared_ty>))
                } else {
                    syn::PathArguments::AngleBracketed(syn::parse_quote!(<#shared_ty, #local_ty>))
                };
            }
        }
        Type::Reference(type_ref) => {
            rewrite_context_type(&mut type_ref.elem, shared_ty, local_ty);
        }
        _ => {}
    }
}

fn rewrite_shared_context_name(ty: &mut Type) {
    match ty {
        Type::Path(path) => {
            if let Some(segment) = path.path.segments.last_mut()
                && segment.ident == "Context"
                && segment.arguments.is_empty()
            {
                segment.ident = format_ident!("SharedContext");
            }
        }
        Type::Reference(reference) => rewrite_shared_context_name(&mut reference.elem),
        _ => {}
    }
}
