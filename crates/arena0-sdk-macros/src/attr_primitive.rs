use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Data, DeriveInput, Error, Fields, Ident, Result, Token};

#[derive(Default)]
pub(crate) struct PrimitiveArgs {
    capabilities: TokenStream,
}

impl Parse for PrimitiveArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self::default();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if key != "capabilities" {
                return Err(Error::new(
                    key.span(),
                    "unsupported arena0::primitive argument; expected `capabilities(...)`",
                ));
            }

            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                args.capabilities = input.parse()?;
            } else {
                let content;
                syn::parenthesized!(content in input);
                args.capabilities = content.parse()?;
            }

            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(args)
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn expand(args: PrimitiveArgs, input: DeriveInput) -> Result<TokenStream> {
    let ident = &input.ident;
    let vis = &input.vis;
    let capabilities = &args.capabilities;
    crate::schema::validate_contract_attrs(&input.attrs)?;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let Data::Struct(data) = &input.data else {
        return Err(Error::new(
            ident.span(),
            "arena0::primitive only supports structs",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(Error::new(
            ident.span(),
            "arena0::primitive requires named fields",
        ));
    };

    let mut struct_fields = Vec::new();
    let mut field_types = Vec::new();

    for field in &fields.named {
        let field_ident = field.ident.as_ref().unwrap();
        let field_ty = &field.ty;
        let field_vis = &field.vis;
        crate::schema::validate_contract_attrs(&field.attrs)?;
        if field.attrs.iter().any(|attribute| {
            attribute.path().is_ident("private") || attribute.path().is_ident("secret")
        }) {
            return Err(Error::new(
                field_ident.span(),
                "inline #[private]/#[secret] fields are unsupported; move participant-local state to Program::Local",
            ));
        }
        // Privacy is explicit at the program boundary: participant-local
        // fields belong in `Program::Local`, never in a shared primitive.
        // Strip only the marker (which was rejected above), keeping ordinary
        // serde and derive-helper attributes intact.
        let clean_attrs: Vec<_> = field.attrs.iter().collect();
        field_types.push(field_ty.clone());

        struct_fields.push(quote! {
            #(#clean_attrs)*
            #field_vis #field_ident: #field_ty
        });
    }

    let remaining_attrs: Vec<_> = input.attrs.iter().collect();
    let generics = &input.generics;

    // Build an extended where clause for the marker implementation. The
    // generated stock Borsh/Serde impls carry their own derive bounds; the
    // marker still needs explicit bounds for generic parameters used by its
    // fields because a composite bound such as `Vec<Option<T>>: Borsh...`
    // does not prove the derive's `T: Borsh...` requirement to Rust.
    let generic_field_params = crate::attr_type::infer_generic_field_params(&input);
    let primitive_where = if generic_field_params.is_empty() {
        quote! { #where_clause }
    } else {
        let existing = where_clause
            .map(|w| {
                let predicates = &w.predicates;
                quote! { #predicates, }
            })
            .unwrap_or_default();
        quote! {
            where #existing
                #(#generic_field_params: ::arena0::borsh::BorshSerialize + ::arena0::borsh::BorshDeserialize),*
        }
    };

    let program_value_impl =
        crate::schema::expand_program_value_impl(ident, &input.generics, field_types);

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
        #(#remaining_attrs)*
        #vis struct #ident #generics #where_clause {
            #(#struct_fields),*
        }

        impl #impl_generics ::arena0::Primitive for #ident #ty_generics #primitive_where {
            fn required_capabilities() -> ::std::vec::Vec<::arena0::Capability> {
                ::arena0::__arena0_capability_vec!(#capabilities)
            }
        }

        #program_value_impl
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_private_fields_are_rejected() {
        let input: DeriveInput = syn::parse_quote! {
            #[derive(Default)]
            struct Primitive {
                visible: u32,
                #[private]
                secret: String,
            }
        };

        let error = expand(PrimitiveArgs::default(), input).unwrap_err();
        assert!(error.to_string().contains("move participant-local state"));
    }

    #[test]
    fn primitive_uses_unconditional_stock_borsh() {
        let input: DeriveInput = syn::parse_quote! {
            #[derive(Default)]
            struct Primitive {
                visible: u32,
            }
        };

        let expanded = expand(PrimitiveArgs::default(), input).unwrap().to_string();
        assert!(expanded.contains("BorshSerialize"));
        assert!(expanded.contains("BorshDeserialize"));
        assert!(!expanded.contains("shared_write"));
        assert!(!expanded.contains("shared_read"));
        assert!(!expanded.contains("__shared"));
    }
}
