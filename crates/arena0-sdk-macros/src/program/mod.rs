//! The `#[arena0::program]` attribute expansion.
//!
//! This module coordinates the inline-module program form. The seams live in
//! submodules:
//!
//! - [`args`]: attribute parsing and program metadata.
//! - [`guest_abi`]: resident-compatible guest ABI export emission.
//! - [`module_shell`]: inline-`mod` shell expansion.
//! - [`capabilities`]: effect-capability inference for `capabilities(auto)`.

use proc_macro2::TokenStream as TokenStream2;
use syn::{Error, Item, Result, Type, spanned::Spanned};

mod args;
mod capabilities;
mod guest_abi;
mod module_shell;

#[cfg(test)]
mod tests;

use crate::util::to_pascal_case;
pub(crate) use args::Arena0ProgramArgs;
use module_shell::expand_arena0_program_module;

pub(crate) fn expand_arena0_program_item(
    args: Arena0ProgramArgs,
    item: Item,
) -> Result<TokenStream2> {
    match item {
        Item::Mod(item) => expand_arena0_program_module(args, item),
        Item::Impl(item) => Err(Error::new(
            item.span(),
            "arena0::program only supports an inline module shell; impl Program blocks are no longer accepted",
        )),
        other => Err(Error::new(
            other.span(),
            "arena0::program can only annotate an inline module shell",
        )),
    }
}

/// Recursively rewrite bare context types to their generic forms inside a type.
fn rewrite_context_type(ty: &mut Type, shared_ty: &Type, local_ty: &Type) {
    match ty {
        Type::Path(type_path) => {
            if let Some(segment) = type_path.path.segments.last_mut()
                && segment.ident == "Context"
                && segment.arguments.is_empty()
            {
                segment.arguments =
                    syn::PathArguments::AngleBracketed(syn::parse_quote!(<#shared_ty, #local_ty>));
            }
        }
        Type::Reference(type_ref) => {
            rewrite_context_type(&mut type_ref.elem, shared_ty, local_ty);
        }
        _ => {}
    }
}
