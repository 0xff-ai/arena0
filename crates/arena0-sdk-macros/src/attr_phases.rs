use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Error, ItemEnum, LitStr, Result};

use crate::util::{parse_phase_attr_optional, to_kebab_case};

pub(crate) fn expand_arena0_phases(item: ItemEnum) -> Result<TokenStream2> {
    let enum_ident = &item.ident;
    crate::schema::validate_contract_attrs(&item.attrs)?;
    let mut decls = Vec::new();
    let mut names = Vec::new();
    let mut terminals = Vec::new();
    let mut defaults = Vec::new();
    let mut variant_docs = Vec::new();
    let mut default_variant = None;

    for variant in &item.variants {
        crate::schema::validate_contract_attrs(&variant.attrs)?;
        for field in &variant.fields {
            crate::schema::validate_contract_attrs(&field.attrs)?;
        }
        let attrs = parse_phase_attr_optional(&variant.attrs)?;
        let ident = &variant.ident;
        let phase_name = attrs
            .name
            .map_or_else(|| to_kebab_case(&ident.to_string()), |lit| lit.value());
        let description = attrs
            .description
            .map_or_else(|| ident.to_string(), |lit| lit.value());
        let is_terminal = attrs.terminal;
        let is_default = attrs.default;

        if is_default {
            if default_variant.is_some() {
                return Err(Error::new(
                    ident.span(),
                    "only one phase variant can be marked default",
                ));
            }
            default_variant = Some(ident.clone());
        }

        decls.push(quote! {
            ::arena0::PhaseDecl {
                name: #phase_name,
                description: #description,
                is_default: #is_default,
                is_terminal: #is_terminal,
            }
        });
        names.push(quote! { Self::#ident => #phase_name });
        terminals.push(quote! { Self::#ident => #is_terminal });
        defaults.push(quote! { Self::#ident => #is_default });
        variant_docs.push(format!(
            "Lifecycle phase `{phase_name}`. Move into this phase by returning `Transition::To({enum_ident}::{ident})`."
        ));
    }

    let default_impl = default_variant.map(|variant| {
        quote! {
            impl ::core::default::Default for #enum_ident {
                fn default() -> Self {
                    Self::#variant
                }
            }
        }
    });

    let mut clean_item = item.clone();
    for (variant, doc) in clean_item.variants.iter_mut().zip(variant_docs) {
        variant.attrs.retain(|attr| !attr.path().is_ident("phase"));
        let doc = LitStr::new(&doc, variant.ident.span());
        variant.attrs.push(syn::parse_quote!(#[doc = #doc]));
    }
    let enum_doc = format!(
        "Lifecycle phase enum for `{enum_ident}`. Generated metadata is available through Arena0Phase::DECLS, and handlers should change phases with Transition::To."
    );
    let program_value_impl = crate::schema::expand_program_value_impl(
        enum_ident,
        &item.generics,
        item.variants
            .iter()
            .flat_map(|variant| variant.fields.iter().map(|field| field.ty.clone())),
    );

    Ok(quote! {
        #[doc = #enum_doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq,
            ::arena0::serde::Serialize,
            ::arena0::serde::Deserialize,
            ::arena0::borsh::BorshSerialize,
            ::arena0::borsh::BorshDeserialize,
            ::arena0::borsh::BorshSchema,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        #clean_item

        impl ::arena0::Arena0Phase for #enum_ident {
            const DECLS: &'static [::arena0::PhaseDecl] = &[#(#decls),*];

            fn as_str(self) -> &'static str {
                match self {
                    #(#names),*
                }
            }

            fn is_terminal(self) -> bool {
                match self {
                    #(#terminals),*
                }
            }

            fn is_default(self) -> bool {
                match self {
                    #(#defaults),*
                }
            }
        }

        #program_value_impl

        #default_impl
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_phases_include_transition_rustdoc() {
        let item: ItemEnum = syn::parse_quote! {
            pub enum Phase {
                #[phase(default)]
                Setup,
                #[phase(name = "reveal")]
                Reveal,
            }
        };

        let expanded = expand_arena0_phases(item).unwrap().to_string();
        assert!(expanded.contains("Lifecycle phase enum for `Phase`"));
        assert!(
            expanded.contains("Move into this phase by returning `Transition::To(Phase::Setup)`")
        );
        assert!(
            expanded.contains("Move into this phase by returning `Transition::To(Phase::Reveal)`")
        );
    }
}
