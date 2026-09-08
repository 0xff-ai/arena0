use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, DeriveInput, Fields, Generics, Ident, Result, Token, Type, WherePredicate};

/// Add the bounds that keep generated values inside the SDK's public value set.
///
/// `ProgramValue` itself owns the paired JSON and Borsh contract. The generated
/// implementations add a bound for each field type so a type that merely has
/// upstream schema implementations does not become ABI-visible by accident.
pub(crate) fn expand_program_value_impl(
    ident: &Ident,
    generics: &Generics,
    field_types: impl IntoIterator<Item = Type>,
) -> TokenStream2 {
    expand_program_value_impl_with_bounds(ident, generics, field_types, std::iter::empty())
}

/// Generate a `ProgramValue` implementation for a type path in a nested module.
pub(crate) fn expand_program_value_impl_for_type(
    type_path: TokenStream2,
    generics: &Generics,
    field_types: impl IntoIterator<Item = Type>,
) -> TokenStream2 {
    expand_program_value_impl_for_type_with_bounds(
        type_path,
        generics,
        field_types,
        std::iter::empty(),
    )
}

/// Generate a `ProgramValue` implementation with explicit caller-provided bounds.
pub(crate) fn expand_program_value_impl_with_bounds(
    ident: &Ident,
    generics: &Generics,
    field_types: impl IntoIterator<Item = Type>,
    extra_bounds: impl IntoIterator<Item = WherePredicate>,
) -> TokenStream2 {
    expand_program_value_impl_for_type_with_bounds(
        quote!(#ident),
        generics,
        field_types,
        extra_bounds,
    )
}

fn expand_program_value_impl_for_type_with_bounds(
    type_path: TokenStream2,
    generics: &Generics,
    field_types: impl IntoIterator<Item = Type>,
    extra_bounds: impl IntoIterator<Item = WherePredicate>,
) -> TokenStream2 {
    let field_types: Vec<Type> = field_types.into_iter().collect();
    let mut generics = generics.clone();
    let mut seen = std::collections::BTreeSet::new();

    for ty in &field_types {
        let key = quote!(#ty).to_string();
        if seen.insert(key) {
            generics
                .make_where_clause()
                .predicates
                .push(syn::parse_quote!(#ty: ::arena0::ProgramValue));
        }
    }

    let type_params: Vec<_> = generics
        .type_params()
        .map(|param| param.ident.clone())
        .collect();
    for param in type_params {
        if field_types.iter().any(|ty| type_mentions_ident(ty, &param)) {
            generics
                .make_where_clause()
                .predicates
                .push(syn::parse_quote!(#param: ::arena0::ProgramValue));
        }
    }

    generics.make_where_clause().predicates.extend(extra_bounds);

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    quote! {
        impl #impl_generics ::arena0::ProgramValue for #type_path #ty_generics #where_clause {}
    }
}

fn type_mentions_ident(ty: &Type, ident: &Ident) -> bool {
    struct Finder<'a> {
        ident: &'a Ident,
        found: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for Finder<'_> {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            if path.segments.len() == 1
                && path
                    .segments
                    .first()
                    .is_some_and(|segment| segment.ident == *self.ident)
            {
                self.found = true;
            }
            syn::visit::visit_path(self, path);
        }
    }

    let mut finder = Finder {
        ident,
        found: false,
    };
    syn::visit::Visit::visit_type(&mut finder, ty);
    finder.found
}

/// Validate attributes that could make JSON metadata and the binary contract
/// describe different fields or layouts.
///
/// The authoring macros add their own `skip` and `crate` attributes after this
/// check. Only bound and crate settings are accepted from callers because they
/// do not change the encoded value shape. Shape-changing settings must use an
/// SDK value type instead.
pub(crate) fn validate_contract_attrs(attrs: &[Attribute]) -> Result<()> {
    for attr in attrs {
        let (kind, allowed) = if attr.path().is_ident("serde") {
            ("serde", &["bound", "crate"][..])
        } else if attr.path().is_ident("borsh") {
            ("borsh", &["bound", "crate"][..])
        } else if attr.path().is_ident("schemars") {
            ("schemars", &["bound", "crate"][..])
        } else {
            continue;
        };

        attr.parse_nested_meta(|meta| {
            if allowed.iter().any(|name| meta.path.is_ident(name)) {
                if meta.input.peek(Token![=]) {
                    let _: syn::Expr = meta.value()?.parse()?;
                } else if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let _: TokenStream2 = content.parse()?;
                }
                return Ok(());
            }
            Err(meta.error(format!(
                "{kind} attribute changes the program-value shape; use the SDK field type instead"
            )))
        })?;
    }
    Ok(())
}

/// Collect all value field types from a derive input in declaration order.
pub(crate) fn field_types(input: &DeriveInput) -> Vec<Type> {
    match &input.data {
        syn::Data::Struct(data) => fields_types(&data.fields),
        syn::Data::Enum(data) => data
            .variants
            .iter()
            .flat_map(|variant| fields_types(&variant.fields))
            .collect(),
        syn::Data::Union(_) => Vec::new(),
    }
}

fn fields_types(fields: &Fields) -> Vec<Type> {
    fields.iter().map(|field| field.ty.clone()).collect()
}

/// Add generic serde bounds when the caller did not provide them.
pub(crate) fn prepare_serde_bounds(input: &mut DeriveInput, inferred: &[Ident]) -> Result<()> {
    if inferred.is_empty() || has_serde_bound(input)? {
        return Ok(());
    }

    let serde_bound: String = inferred
        .iter()
        .map(|id| {
            format!("{id}: ::arena0::serde::Serialize + ::arena0::serde::de::DeserializeOwned")
        })
        .collect::<Vec<_>>()
        .join(", ");
    input
        .attrs
        .push(syn::parse_quote!(#[serde(bound = #serde_bound)]));
    Ok(())
}

fn has_serde_bound(input: &DeriveInput) -> Result<bool> {
    for attr in input
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("serde"))
    {
        let mut found = false;
        attr.parse_nested_meta(|meta| {
            found |= meta.path.is_ident("bound");
            if meta.input.peek(Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            } else if meta.input.peek(syn::token::Paren) {
                let content;
                syn::parenthesized!(content in meta.input);
                let _: TokenStream2 = content.parse()?;
            }
            Ok(())
        })?;
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_changing_schemars_attributes_are_rejected() {
        let attrs: Vec<Attribute> = vec![syn::parse_quote!(#[schemars(skip)])];
        let error = validate_contract_attrs(&attrs).unwrap_err();
        assert!(error.to_string().contains("schemars attribute changes"));
    }

    #[test]
    fn shape_changing_defaults_are_rejected() {
        let attrs: Vec<Attribute> = vec![syn::parse_quote!(#[serde(default)])];
        let error = validate_contract_attrs(&attrs).unwrap_err();
        assert!(error.to_string().contains("serde attribute changes"));
    }

    #[test]
    fn serde_crate_path_containing_bound_does_not_suppress_inferred_bounds() {
        let mut input: DeriveInput = syn::parse_quote! {
            #[serde(crate = "::my_bound_serde")]
            struct Value<T> {
                value: T,
            }
        };
        let inferred = [syn::parse_str::<Ident>("T").expect("generic ident")];

        prepare_serde_bounds(&mut input, &inferred).unwrap();

        assert_eq!(input.attrs.len(), 2);
        let actual = &input.attrs[1];
        let expected: Attribute = syn::parse_quote! {
            #[serde(bound = "T: ::arena0::serde::Serialize + ::arena0::serde::de::DeserializeOwned")]
        };
        assert_eq!(quote!(#actual).to_string(), quote!(#expected).to_string());
    }

    #[test]
    fn explicit_serde_bound_forms_are_preserved_once() {
        let attrs: [Attribute; 2] = [
            syn::parse_quote!(#[serde(bound = "T: Serialize")]),
            syn::parse_quote!(#[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]),
        ];
        for attr in attrs {
            let mut input: DeriveInput = syn::parse_quote! {
                #attr
                struct Value<T> { value: T }
            };
            validate_contract_attrs(&input.attrs).unwrap();
            prepare_serde_bounds(&mut input, &[syn::parse_quote!(T)]).unwrap();
            assert_eq!(input.attrs.len(), 1);
            let actual = &input.attrs[0];
            assert_eq!(quote!(#actual).to_string(), quote!(#attr).to_string());
        }
    }
}
