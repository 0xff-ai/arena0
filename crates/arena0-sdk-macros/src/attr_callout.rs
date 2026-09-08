use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Error, Fields, FieldsNamed, Ident, ItemEnum, LitStr, Result, Type, spanned::Spanned};

use crate::util::to_kebab_case;

struct CalloutVariantAttr {
    prompt: Option<LitStr>,
    output_ty: Option<Type>,
}

struct CalloutVariantParsed {
    ident: Ident,
    doc_comment: Option<String>,
    attr: CalloutVariantAttr,
    context_fields: FieldsNamed,
    output_ty: Type,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn expand_arena0_callouts(item: ItemEnum) -> Result<TokenStream2> {
    let request_ident = &item.ident;
    crate::schema::validate_contract_attrs(&item.attrs)?;

    // Derive input enum name. The answer side stays "input": the program calls
    // out, the agent supplies the input.
    let request_name = request_ident.to_string();
    let response_name = if request_name == "Callout" {
        "Input".to_string()
    } else if let Some(base) = request_name.strip_suffix("Callout") {
        format!("{base}Input")
    } else if let Some(base) = request_name.strip_suffix("Request") {
        format!("{base}Input")
    } else {
        format!("{request_name}Input")
    };
    let response_ident = format_ident!("{response_name}");

    let mut parsed_variants = Vec::new();

    for variant in &item.variants {
        crate::schema::validate_contract_attrs(&variant.attrs)?;
        for field in &variant.fields {
            crate::schema::validate_contract_attrs(&field.attrs)?;
        }
        let doc_comment = extract_doc_comment(&variant.attrs);
        let attr = parse_callout_variant_attr(&variant.attrs)?;
        let output_ty = attr
            .output_ty
            .clone()
            .unwrap_or_else(|| syn::parse_quote!(String));
        let context_fields = match &variant.fields {
            Fields::Named(named) => named.clone(),
            Fields::Unit => syn::parse_quote!({}),
            Fields::Unnamed(_) => {
                return Err(Error::new(
                    variant.span(),
                    "arena0::callouts variants must use named fields or be unit",
                ));
            }
        };
        parsed_variants.push(CalloutVariantParsed {
            ident: variant.ident.clone(),
            doc_comment,
            attr,
            context_fields,
            output_ty,
        });
    }

    let mut response_variants = Vec::new();
    let mut request_variants = Vec::new();
    let mut schema_entries = Vec::new();
    let mut request_callout_index_arms = Vec::new();
    let mut request_serialize_arms = Vec::new();
    let mut from_raw_arms = Vec::new();
    let mut to_event_data_arms = Vec::new();
    let mut named_request_structs = Vec::new();
    let mut named_request_from_impls = Vec::new();
    let mut named_request_impls = Vec::new();
    let mut callout_spec_impls = Vec::new();
    let mut request_types = Vec::new();
    let mut response_types = Vec::new();
    let mut named_request_program_value_impls = Vec::new();

    for (idx, v) in parsed_variants.iter().enumerate() {
        let variant_ident = &v.ident;
        let output_ty = &v.output_ty;
        let idx_u32 = idx as u32;
        let name = variant_ident.to_string();

        let output_ty_name = quote! { #output_ty }.to_string();
        response_types.push(output_ty.clone());
        request_types.extend(v.context_fields.named.iter().map(|field| field.ty.clone()));
        let request_variant_doc = format!(
            "Arena-owned callout `{name}`. Await this request to produce an `{output_ty_name}` response."
        );
        let response_variant_doc =
            format!("Input response for the `{name}` callout, carrying `{output_ty_name}`.");
        let request_struct_doc = format!(
            "Typed request payload for callout `{name}`. `ctx.effects().callout(callouts::{name} {{ ... }}).await` resumes with `{output_ty_name}`."
        );

        // Response variant: VariantName(OutputType)
        response_variants.push(quote! {
            #[doc = #response_variant_doc]
            #variant_ident(#output_ty)
        });

        // Request variant: preserved from source
        let fields = &v.context_fields;
        request_variants.push(quote! {
            #[doc = #request_variant_doc]
            #variant_ident #fields
        });

        // Prompt: doc comment > attribute > kebab-case name
        let prompt = v
            .doc_comment
            .clone()
            .or_else(|| v.attr.prompt.as_ref().map(LitStr::value))
            .unwrap_or_else(|| to_kebab_case(&variant_ident.to_string()).replace('-', " "));

        schema_entries.push(quote! {
            ::arena0::CalloutSchema {
                name: #name.into(),
                prompt: #prompt.into(),
                input: <callouts::#variant_ident as ::arena0::ProgramValue>::json_schema(),
                output: <#output_ty as ::arena0::ProgramValue>::json_schema(),
            }
        });

        // Request callout_index arms
        let field_names: Vec<_> = v
            .context_fields
            .named
            .iter()
            .map(|f| f.ident.as_ref().unwrap())
            .collect();
        let mut named_struct_fields = v.context_fields.clone();
        for field in &mut named_struct_fields.named {
            field.vis = syn::parse_quote!(pub);
        }
        named_request_structs.push(quote! {
            #[doc = #request_struct_doc]
            #[derive(
                Debug,
                Clone,
                ::arena0::borsh::BorshSerialize,
                ::arena0::borsh::BorshDeserialize,
                ::arena0::schemars::JsonSchema,
                ::arena0::serde::Serialize,
                ::arena0::serde::Deserialize,
            )]
            #[schemars(crate = "::arena0::schemars")]
            pub struct #variant_ident #named_struct_fields
        });
        named_request_program_value_impls.push(crate::schema::expand_program_value_impl_for_type(
            quote!(callouts::#variant_ident),
            &syn::Generics::default(),
            v.context_fields.named.iter().map(|field| field.ty.clone()),
        ));

        named_request_from_impls.push(quote! {
            impl From<callouts::#variant_ident> for #request_ident {
                fn from(request: callouts::#variant_ident) -> Self {
                    let callouts::#variant_ident { #(#field_names),* } = request;
                    #request_ident::#variant_ident { #(#field_names),* }
                }
            }
        });

        named_request_impls.push(quote! {
            impl ::arena0::Arena0CalloutRequest for callouts::#variant_ident {
                fn callout_index(&self) -> u32 {
                    #idx_u32
                }

                fn expected_type_name(&self) -> Option<&'static str> {
                    Some(::core::any::type_name::<#output_ty>())
                }
            }

            impl ::arena0::Arena0TypedCalloutRequest for callouts::#variant_ident {
                type Output = #output_ty;
            }
        });

        callout_spec_impls.push(quote! {
            impl<P> ::arena0::CalloutSpec<P> for callouts::#variant_ident
            where
                P: ::arena0::Program<Callout = #request_ident, Input = #response_ident>,
            {
                const CALLOUT_INDEX: u32 = #idx_u32;
                const CALLOUT_NAME: &'static str = stringify!(#variant_ident);

                type Output = #output_ty;

                fn into_callout(self) -> P::Callout {
                    self.into()
                }

                fn into_input(output: Self::Output) -> P::Input {
                    #response_ident::#variant_ident(output)
                }

                fn decode(input: P::Input) -> Result<Self::Output, ::arena0::InputFault> {
                    use ::arena0::anyhow::anyhow;

                    match input {
                        #response_ident::#variant_ident(value) => Ok(value),
                        _ => Err(::arena0::InputFault::Unrecoverable(
                            anyhow!(
                                "expected input variant {} for callout {}",
                                stringify!(#variant_ident),
                                stringify!(#variant_ident),
                            ),
                        )),
                    }
                }
            }
        });

        let ignore_fields = if field_names.is_empty() {
            quote! { {} }
        } else {
            quote! { { #(#field_names: _),* } }
        };
        request_callout_index_arms.push(quote! {
            #request_ident::#variant_ident #ignore_fields => #idx_u32
        });

        // Request serialize arms
        let field_refs: Vec<_> = v
            .context_fields
            .named
            .iter()
            .map(|f| f.ident.as_ref().unwrap())
            .collect();
        if field_refs.is_empty() {
            request_serialize_arms.push(quote! {
                #request_ident::#variant_ident {} => {
                    serializer.serialize_bytes(&[])
                }
            });
        } else {
            request_serialize_arms.push(quote! {
                #request_ident::#variant_ident { #(#field_refs),* } => {
                    use ::arena0::serde::ser::SerializeStruct;
                    let field_count = [#(stringify!(#field_refs)),*].len();
                    let mut s = serializer.serialize_struct(#name, field_count)?;
                    #(s.serialize_field(stringify!(#field_refs), #field_refs)?;)*
                    s.end()
                }
            });
        }

        // from_raw: build response variant
        from_raw_arms.push(quote! {
            #idx_u32 => {
                let value: #output_ty = ::arena0::__parse_input_data(&data);
                #response_ident::#variant_ident(value)
            }
        });

        // to_event_data: destructure response variant
        to_event_data_arms.push(quote! {
            #response_ident::#variant_ident(value) => {
                (#idx_u32, ::arena0::__serialize_input_data::<#output_ty>(value))
            }
        });
    }

    let request_program_value_impl =
        crate::schema::expand_program_value_impl(request_ident, &item.generics, request_types);
    let response_program_value_impl =
        crate::schema::expand_program_value_impl(&response_ident, &item.generics, response_types);

    Ok(quote! {
        #[doc = concat!("Typed request structs generated for `", stringify!(#request_ident), "` callouts.")]
        pub mod callouts {
            use super::*;

            #(#named_request_structs)*
        }

        #[doc = concat!("Arena-owned callout request enum generated by `#[arena0::callouts]`. Each variant has a typed request struct in `callouts`.")]
        #[derive(
            Debug,
            Clone,
            ::arena0::borsh::BorshSerialize,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        pub enum #request_ident {
            #(#request_variants),*
        }

        #(#named_request_from_impls)*
        #(#named_request_impls)*
        #(#callout_spec_impls)*
        #(#named_request_program_value_impls)*

        impl ::arena0::serde::Serialize for #request_ident {
            fn serialize<S: ::arena0::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    #(#request_serialize_arms),*
                }
            }
        }

        impl ::arena0::Arena0CalloutRequest for #request_ident {
            fn callout_index(&self) -> u32 {
                match self {
                    #(#request_callout_index_arms),*
                }
            }
        }

        impl ::arena0::Arena0TypedCalloutRequest for #request_ident {
            type Output = #response_ident;
        }

        #[doc = concat!("Typed input enum generated from `", stringify!(#request_ident), "`. Each variant maps one callout to its output type.")]
        #[derive(
            Debug,
            Clone,
            ::arena0::serde::Deserialize,
            ::arena0::serde::Serialize,
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

        impl ::arena0::Arena0Callout for #request_ident {
            type Request = #request_ident;
            type Response = #response_ident;

            fn schemas() -> Vec<::arena0::CalloutSchema> {
                vec![#(#schema_entries),*]
            }

            fn from_raw(callout_index: u32, data: Vec<u8>) -> #response_ident {
                match callout_index {
                    #(#from_raw_arms)*
                    _ => panic!("unknown callout index: {callout_index}"),
                }
            }

            fn to_event_data(response: &#response_ident) -> (u32, Vec<u8>) {
                match response {
                    #(#to_event_data_arms)*
                }
            }
        }
    })
}

fn extract_doc_comment(attrs: &[syn::Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("doc")
            && let syn::Meta::NameValue(nv) = &attr.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            lines.push(s.value().trim().to_string());
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join(" "))
    }
}

fn parse_callout_variant_attr(attrs: &[syn::Attribute]) -> Result<CalloutVariantAttr> {
    let mut prompt = None;
    let mut output_ty = None;

    for attr in attrs {
        if !is_arena0_callout_attr(attr) {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("prompt") {
                prompt = Some(meta.value()?.parse()?);
                return Ok(());
            }
            if meta.path.is_ident("output") {
                let value = meta.value()?;
                output_ty = Some(value.parse()?);
                return Ok(());
            }
            Err(meta.error("unsupported callout attribute; expected `prompt` or `output`"))
        })?;
    }

    Ok(CalloutVariantAttr { prompt, output_ty })
}

fn is_arena0_callout_attr(attr: &syn::Attribute) -> bool {
    let mut segments = attr.path().segments.iter();
    let Some(first) = segments.next() else {
        return false;
    };
    let Some(second) = segments.next() else {
        return false;
    };
    segments.next().is_none() && first.ident == "arena0" && second.ident == "callout"
}
