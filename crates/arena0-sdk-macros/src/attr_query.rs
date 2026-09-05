use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Error, Fields, ItemEnum, Result, Type, spanned::Spanned};

use crate::util::to_kebab_case;

pub(crate) fn expand_arena0_query(item: ItemEnum) -> Result<TokenStream2> {
    let enum_ident = &item.ident;
    let response_ident = format_ident!("{}Response", enum_ident);
    let mut query_variants = Vec::new();
    let mut response_variants = Vec::new();
    let mut request_types = Vec::new();
    let mut response_types = Vec::new();
    let query_label = to_kebab_case(&enum_ident.to_string()).replace('-', " ");

    crate::schema::validate_contract_attrs(&item.attrs)?;

    for variant in &item.variants {
        crate::schema::validate_contract_attrs(&variant.attrs)?;
        for field in &variant.fields {
            crate::schema::validate_contract_attrs(&field.attrs)?;
        }
        let ident = &variant.ident;
        let response_ty = extract_query_response_type(variant)?;

        let request_ty = match &variant.fields {
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                fields.unnamed.first().unwrap().ty.clone()
            }
            Fields::Unit => syn::parse_quote!(()),
            _ => {
                return Err(Error::new(
                    variant.span(),
                    "arena0::query variants must have exactly one unnamed field or be unit",
                ));
            }
        };

        query_variants.push(quote! {
            #ident(#request_ty)
        });
        response_variants.push(quote! {
            #ident(#response_ty)
        });
        request_types.push(request_ty.clone());
        response_types.push(response_ty.clone());
    }

    let request_program_value_impl =
        crate::schema::expand_program_value_impl(enum_ident, &item.generics, request_types);
    let response_program_value_impl =
        crate::schema::expand_program_value_impl(&response_ident, &item.generics, response_types);

    Ok(quote! {
        #[derive(
            Debug,
            Clone,
            ::arena0::serde::Serialize,
            ::arena0::serde::Deserialize,
            ::arena0::borsh::BorshSerialize,
            ::arena0::borsh::BorshDeserialize,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        pub enum #enum_ident {
            #(#query_variants),*
        }

        #[derive(
            Debug,
            Clone,
            ::arena0::serde::Serialize,
            ::arena0::serde::Deserialize,
            ::arena0::borsh::BorshSerialize,
            ::arena0::borsh::BorshDeserialize,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        pub enum #response_ident {
            #(#response_variants),*
        }

        #request_program_value_impl
        #response_program_value_impl

        impl ::arena0::Arena0Query for #enum_ident {
            type Response = #response_ident;
            fn schemas() -> ::std::vec::Vec<::arena0::QuerySchema> {
                ::std::vec![::arena0::QuerySchema {
                    name: stringify!(#enum_ident).into(),
                    label: #query_label.into(),
                    request: <#enum_ident as ::arena0::ProgramValue>::json_schema(),
                    response: <#response_ident as ::arena0::ProgramValue>::json_schema(),
                }]
            }
        }
    })
}

fn extract_query_response_type(variant: &syn::Variant) -> Result<Type> {
    for attr in &variant.attrs {
        if attr.path().is_ident("query") {
            let mut response_ty = None;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("response") {
                    response_ty = Some(meta.value()?.parse()?);
                    return Ok(());
                }
                Err(meta.error("unsupported query attribute"))
            })?;
            if let Some(ty) = response_ty {
                return Ok(ty);
            }
        }
    }
    Ok(syn::parse_quote!(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_schema_describes_the_aggregate_wire_enums() {
        let item: ItemEnum = syn::parse_quote! {
            enum StateQuery {
                #[query(response = u32)]
                Count,
                #[query(response = String)]
                Name(u8),
            }
        };

        let expanded = expand_arena0_query(item).unwrap().to_string();

        assert_eq!(expanded.matches(":: arena0 :: QuerySchema {").count(), 1);
        assert!(expanded.contains("StateQuery as :: arena0 :: ProgramValue"));
        assert!(expanded.contains("StateQueryResponse as :: arena0 :: ProgramValue"));
        assert!(!expanded.contains("stringify ! (Count)"));
        assert!(!expanded.contains("stringify ! (Name)"));
    }

    #[test]
    fn unsupported_variant_attributes_are_rejected() {
        let item: ItemEnum = syn::parse_quote! {
            enum StateQuery {
                #[query(label = "count", response = u32)]
                Count,
            }
        };

        let error = expand_arena0_query(item).unwrap_err();

        assert!(error.to_string().contains("unsupported query attribute"));
    }
}
