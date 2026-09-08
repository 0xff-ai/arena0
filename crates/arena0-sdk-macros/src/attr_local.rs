use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::{Data, DeriveInput, Error, Fields, Result};

pub(crate) fn expand_arena0_local(mut input: DeriveInput) -> Result<TokenStream2> {
    let ident = &input.ident;
    let has_borsh_serialize = has_derive(&input, "BorshSerialize")?;
    let has_borsh_deserialize = has_derive(&input, "BorshDeserialize")?;
    let debug_derived = has_derive(&input, "Debug")?;
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

    if debug_derived {
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

fn has_derive(input: &DeriveInput, name: &str) -> Result<bool> {
    let mut found = false;
    for attr in input
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("derive"))
    {
        let derives =
            attr.parse_args_with(Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)?;
        found |= derives.iter().any(|path| {
            path.segments
                .last()
                .is_some_and(|segment| segment.ident == name)
        });
    }
    Ok(found)
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
    fn qualified_derives_drive_expansion_without_duplicates() {
        let input: DeriveInput = syn::parse_quote! {
            #[derive(
                ::borsh::BorshSerialize,
                ::borsh::BorshDeserialize
            )]
            pub struct Local {
                value: u32,
            }
        };

        let expanded = expand_arena0_local(input).unwrap().to_string();
        assert_eq!(expanded.matches("BorshSerialize").count(), 1);
        assert_eq!(expanded.matches("BorshDeserialize").count(), 1);
    }

    #[test]
    fn debug_derives_are_rejected_for_local() {
        let inputs: [DeriveInput; 3] = [
            syn::parse_quote! {
                #[derive(Default, Debug)]
                pub struct Local {
                    #[secret]
                    salt: [u8; 32],
                }
            },
            syn::parse_quote! {
                #[derive(::core::fmt::Debug)]
                pub struct Local {
                    #[secret]
                    salt: [u8; 32],
                }
            },
            syn::parse_quote! {
                #[derive(::core::fmt::Debug)]
                pub struct Local {
                    value: u32,
                }
            },
        ];

        for input in inputs {
            let err = expand_arena0_local(input).unwrap_err();
            assert!(err.to_string().contains("redacted Debug"));
        }
    }

    #[test]
    fn near_collision_derives_do_not_change_local_expansion() {
        let inputs: [DeriveInput; 2] = [
            syn::parse_quote! {
                #[derive(DebugExtra)]
                pub struct Local {
                    #[secret]
                    salt: [u8; 32],
                }
            },
            syn::parse_quote! {
                #[derive(DebugExtra, BorshSerializeExtra, BorshDeserializeExtra)]
                pub struct Local {
                    #[secret]
                    salt: [u8; 32],
                }
            },
        ];

        for input in inputs {
            let expanded = expand_arena0_local(input).unwrap().to_string();
            assert!(expanded.contains(":: arena0 :: borsh :: BorshSerialize"));
            assert!(expanded.contains(":: arena0 :: borsh :: BorshDeserialize"));
            assert!(expanded.contains("\"[redacted]\""));
        }
    }

    #[test]
    fn malformed_derive_input_is_rejected() {
        let input: DeriveInput = syn::parse_quote! {
            #[derive(BorshSerialize)]
            #[derive(Debug = "invalid")]
            pub struct Local {
                value: u32,
            }
        };

        assert!(expand_arena0_local(input).is_err());
    }
}
