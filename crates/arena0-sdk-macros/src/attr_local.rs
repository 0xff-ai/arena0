use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{Data, DeriveInput, Error, Fields, Result};

pub(crate) fn expand_arena0_local(mut input: DeriveInput) -> Result<TokenStream2> {
    let ident = &input.ident;
    let has_borsh_serialize = derives_named(&input, "BorshSerialize");
    let has_borsh_deserialize = derives_named(&input, "BorshDeserialize");
    let debug_derived = derives_debug(&input);
    let fields = match &mut input.data {
        Data::Struct(data) => match &mut data.fields {
            Fields::Named(fields) => fields,
            _ => {
                return Err(Error::new_spanned(
                    &input,
                    "#[arena0::local] currently supports structs with named fields",
                ));
            }
        },
        _ => {
            return Err(Error::new_spanned(
                &input,
                "#[arena0::local] supports structs only",
            ));
        }
    };

    let has_secret = fields.named.iter().any(|field| {
        field
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("secret"))
    });
    if has_secret && debug_derived {
        return Err(Error::new_spanned(
            &input,
            "#[arena0::local] generates redacted Debug; remove derive(Debug) from Local",
        ));
    }

    let mut debug_fields = Vec::new();
    let mut snapshot_fields = Vec::new();
    for field in &mut fields.named {
        let name = field.ident.as_ref().expect("named field");
        let field_name = name.to_string();
        let is_secret = field
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("secret"));
        field.attrs.retain(|attr| !attr.path().is_ident("secret"));

        if is_secret {
            debug_fields.push(quote! {
                .field(#field_name, &"[redacted]")
            });
            snapshot_fields.push(quote! {
                object.insert(
                    #field_name.to_owned(),
                    ::arena0::serde_json::Value::String("[redacted]".to_owned()),
                );
            });
        } else {
            debug_fields.push(quote! {
                .field(#field_name, &self.#name)
            });
            snapshot_fields.push(quote! {
                object.insert(
                    #field_name.to_owned(),
                    ::arena0::serde_json::to_value(&self.#name)
                        .unwrap_or_else(|_| ::arena0::serde_json::Value::String("[unserializable]".to_owned())),
                );
            });
        }
    }

    if !has_borsh_serialize || !has_borsh_deserialize {
        let mut derives = Vec::new();
        if !has_borsh_serialize {
            derives.push(quote! { ::arena0::borsh::BorshSerialize });
        }
        if !has_borsh_deserialize {
            derives.push(quote! { ::arena0::borsh::BorshDeserialize });
        }
        input
            .attrs
            .push(syn::parse_quote! { #[derive(#(#derives),*)] });
    }
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        #input

        impl #impl_generics ::core::fmt::Debug for #ident #ty_generics #where_clause {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.debug_struct(::core::stringify!(#ident))
                    #(#debug_fields)*
                    .finish()
            }
        }

        impl #impl_generics ::arena0::LocalDebug for #ident #ty_generics #where_clause {
            fn local_debug_snapshot(&self) -> ::arena0::serde_json::Value {
                let mut object = ::arena0::serde_json::Map::new();
                #(#snapshot_fields)*
                ::arena0::serde_json::Value::Object(object)
            }
        }
    })
}

fn derives_debug(input: &DeriveInput) -> bool {
    input.attrs.iter().any(|attr| {
        attr.path().is_ident("derive") && attr.meta.to_token_stream().to_string().contains("Debug")
    })
}

fn derives_named(input: &DeriveInput, name: &str) -> bool {
    input.attrs.iter().any(|attr| {
        attr.path().is_ident("derive") && attr.meta.to_token_stream().to_string().contains(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_secret_fields_are_redacted() {
        let input: DeriveInput = syn::parse_quote! {
            #[derive(Default)]
            pub struct Local {
                #[secret]
                salt: [u8; 32],
                pending: Option<u32>,
            }
        };

        let expanded = expand_arena0_local(input).unwrap().to_string();
        assert!(expanded.contains("\"[redacted]\""));
        assert!(expanded.contains("impl :: core :: fmt :: Debug for Local"));
        assert!(expanded.contains("impl :: arena0 :: LocalDebug for Local"));
        assert!(!expanded.contains("# [ secret ]"));
    }

    #[test]
    fn local_secret_fields_reject_derived_debug() {
        let input: DeriveInput = syn::parse_quote! {
            #[derive(Default, Debug)]
            pub struct Local {
                #[secret]
                salt: [u8; 32],
            }
        };

        let err = expand_arena0_local(input).unwrap_err();
        assert!(err.to_string().contains("redacted Debug"));
    }
}
