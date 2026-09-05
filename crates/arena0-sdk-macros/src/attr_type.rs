use std::collections::HashSet;

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{DeriveInput, Error, Fields, Result, Token, Type, WherePredicate};

use crate::schema;

#[derive(Default)]
pub(crate) struct ProgramValueArgs {
    pub(crate) bound: Punctuated<WherePredicate, Token![,]>,
}

impl Parse for ProgramValueArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = Self::default();

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "bound" => {
                    let raw: syn::LitStr = input.parse()?;
                    args.bound =
                        raw.parse_with(Punctuated::<WherePredicate, Token![,]>::parse_terminated)?;
                }
                _ => return Err(Error::new(key.span(), "unsupported program-value argument")),
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(args)
    }
}

/// Collect generic type parameter idents that appear in any field type.
pub(crate) fn infer_generic_field_params(input: &DeriveInput) -> Vec<syn::Ident> {
    let all_params: HashSet<&syn::Ident> =
        input.generics.type_params().map(|tp| &tp.ident).collect();
    if all_params.is_empty() {
        return Vec::new();
    }

    let mut found = HashSet::new();

    let fields_iter: Vec<&Fields> = match &input.data {
        syn::Data::Struct(data) => vec![&data.fields],
        syn::Data::Enum(data) => data.variants.iter().map(|v| &v.fields).collect(),
        syn::Data::Union(_) => return Vec::new(),
    };

    for fields in fields_iter {
        for field in fields {
            collect_type_params(&field.ty, &all_params, &mut found);
        }
    }

    // Preserve declaration order from generics
    input
        .generics
        .type_params()
        .filter(|tp| found.contains(&tp.ident))
        .map(|tp| tp.ident.clone())
        .collect()
}

/// Recursively walk a type, recording any references to generic params.
fn collect_type_params<'a>(
    ty: &'a Type,
    generics: &HashSet<&'a syn::Ident>,
    found: &mut HashSet<&'a syn::Ident>,
) {
    match ty {
        Type::Path(type_path) => {
            if let Some(ident) = type_path.path.get_ident()
                && generics.contains(ident)
            {
                found.insert(ident);
                return;
            }
            for segment in &type_path.path.segments {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    for arg in &args.args {
                        if let syn::GenericArgument::Type(inner) = arg {
                            collect_type_params(inner, generics, found);
                        }
                    }
                }
            }
        }
        Type::Tuple(tuple) => {
            for elem in &tuple.elems {
                collect_type_params(elem, generics, found);
            }
        }
        Type::Array(array) => {
            collect_type_params(&array.elem, generics, found);
        }
        Type::Slice(slice) => {
            collect_type_params(&slice.elem, generics, found);
        }
        Type::Reference(reference) => {
            collect_type_params(&reference.elem, generics, found);
        }
        _ => {}
    }
}

/// Infer generic field bounds and add serde bounds when the caller did not
/// provide them. The generated `ProgramValue` implementation receives the
/// parsed explicit bounds separately.
fn prepare(input: &mut DeriveInput) {
    let inferred = infer_generic_field_params(input);
    schema::prepare_serde_bounds(input, &inferred);
}

pub(crate) fn expand_program_value(
    args: ProgramValueArgs,
    mut input: DeriveInput,
) -> Result<TokenStream2> {
    prepare(&mut input);
    validate_input_attrs(&input)?;
    let program_value_impl = schema::expand_program_value_impl_with_bounds(
        &input.ident,
        &input.generics,
        schema::field_types(&input),
        args.bound,
    );

    Ok(quote! {
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            ::arena0::serde::Serialize,
            ::arena0::serde::Deserialize,
            ::arena0::borsh::BorshSerialize,
            ::arena0::borsh::BorshDeserialize,
            ::arena0::borsh::BorshSchema,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        #input

        #program_value_impl
    })
}

pub(crate) fn expand_arena0_message(
    args: ProgramValueArgs,
    mut input: DeriveInput,
) -> Result<TokenStream2> {
    prepare(&mut input);
    validate_input_attrs(&input)?;
    let program_value_impl = schema::expand_program_value_impl_with_bounds(
        &input.ident,
        &input.generics,
        schema::field_types(&input),
        args.bound,
    );

    Ok(quote! {
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            ::arena0::serde::Serialize,
            ::arena0::serde::Deserialize,
            ::arena0::borsh::BorshSerialize,
            ::arena0::borsh::BorshDeserialize,
            ::arena0::borsh::BorshSchema,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        #input

        #program_value_impl
    })
}

fn validate_input_attrs(input: &DeriveInput) -> Result<()> {
    schema::validate_contract_attrs(&input.attrs)?;
    match &input.data {
        syn::Data::Struct(data) => {
            for field in &data.fields {
                schema::validate_contract_attrs(&field.attrs)?;
            }
        }
        syn::Data::Enum(data) => {
            for variant in &data.variants {
                schema::validate_contract_attrs(&variant.attrs)?;
                for field in &variant.fields {
                    schema::validate_contract_attrs(&field.attrs)?;
                }
            }
        }
        syn::Data::Union(data) => {
            for field in &data.fields.named {
                schema::validate_contract_attrs(&field.attrs)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_types_join_the_program_value_allowlist() {
        let input: DeriveInput = syn::parse_quote! {
            pub struct Value {
                number: u32,
                text: String,
            }
        };
        let expanded = expand_program_value(ProgramValueArgs::default(), input)
            .unwrap()
            .to_string();
        assert!(expanded.contains("JsonSchema"));
        assert!(expanded.contains("ProgramValue"));
    }

    #[test]
    fn shape_changing_serde_attributes_are_rejected() {
        let input: DeriveInput = syn::parse_quote! {
            pub struct Value {
                #[serde(rename = "renamed")]
                number: u32,
            }
        };
        let error = expand_program_value(ProgramValueArgs::default(), input).unwrap_err();
        assert!(error.to_string().contains("serde attribute changes"));
    }

    #[test]
    fn shape_changing_borsh_attributes_are_rejected() {
        let input: DeriveInput = syn::parse_quote! {
            pub struct Value {
                #[borsh(skip)]
                number: u32,
            }
        };
        let error = expand_program_value(ProgramValueArgs::default(), input).unwrap_err();
        assert!(error.to_string().contains("borsh attribute changes"));
    }
}
