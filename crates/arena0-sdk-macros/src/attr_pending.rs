use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Error, Fields, ItemEnum, Result};

use crate::util::to_kebab_case;

pub(crate) fn expand_arena0_pending(item: ItemEnum) -> Result<TokenStream2> {
    let enum_ident = &item.ident;
    crate::schema::validate_contract_attrs(&item.attrs)?;
    let mut decls = Vec::new();
    let mut labels = Vec::new();

    for variant in &item.variants {
        crate::schema::validate_contract_attrs(&variant.attrs)?;
        for field in &variant.fields {
            crate::schema::validate_contract_attrs(&field.attrs)?;
        }
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new(
                variant.ident.span(),
                "arena0::pending only supports unit variants",
            ));
        }

        let ident = &variant.ident;
        let label = to_kebab_case(&ident.to_string());
        decls.push(quote! {
            ::arena0::PendingDecl {
                name: #label,
            }
        });
        labels.push(quote! { Self::#ident => #label });
    }

    let program_value_impl = crate::schema::expand_program_value_impl(
        enum_ident,
        &item.generics,
        item.variants
            .iter()
            .flat_map(|variant| variant.fields.iter().map(|field| field.ty.clone())),
    );

    Ok(quote! {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash,
            ::arena0::serde::Serialize,
            ::arena0::serde::Deserialize,
            ::arena0::borsh::BorshSerialize,
            ::arena0::borsh::BorshDeserialize,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        #item

        impl ::arena0::Arena0Pending for #enum_ident {
            const DECLS: &'static [::arena0::PendingDecl] = &[#(#decls),*];

            fn as_str(self) -> &'static str {
                match self {
                    #(#labels),*
                }
            }
        }

        impl ::core::fmt::Display for #enum_ident {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(::arena0::Arena0Pending::as_str(*self))
            }
        }

        #program_value_impl
    })
}
