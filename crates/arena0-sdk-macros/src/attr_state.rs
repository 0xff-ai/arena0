use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use std::collections::BTreeMap;
use syn::parse::{Parse, ParseStream};
use syn::{Data, DeriveInput, Error, Fields, Result, Token};

use crate::util::to_pascal_case;

const RESERVED_PRIMITIVE_ACCESSORS: &[&str] = &[
    "shared",
    "shared_mut",
    "mutate_shared",
    "local",
    "local_mut",
    "effects",
    "crypto",
    "me",
    "other",
    "peer",
    "peer_id",
    "peer_for",
    "participants",
    "participant_for_peer",
    "identity",
    "my_index",
    "random",
    "random_bytes",
];

fn parse_primitive_attr(field: &syn::Field) -> Result<(bool, Option<syn::Path>)> {
    let mut has_primitive = false;
    let mut route = None;

    for attr in field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("primitive"))
    {
        has_primitive = true;
        if matches!(attr.meta, syn::Meta::Path(_)) {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("route") {
                meta.input.parse::<Token![=]>()?;
                if route.replace(meta.input.parse()?).is_some() {
                    return Err(meta.error("duplicate primitive route"));
                }
                Ok(())
            } else {
                Err(meta.error("unsupported primitive argument"))
            }
        })?;
    }

    Ok((has_primitive, route))
}

fn primitive_route_ident(state: &syn::Ident, field: &syn::Ident) -> syn::Ident {
    format_ident!(
        "__Arena0{}{}PrimitiveRoute",
        state,
        to_pascal_case(&field.to_string())
    )
}

fn primitive_output_type(ty: &syn::Type) -> Option<TokenStream2> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident == "FairRandom" {
        return Some(quote! { ::arena0_primitives::commit_reveal::Message<[u8; 32]> });
    }
    if segment.ident != "CommitReveal" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    let arg = args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    })?;
    Some(quote! { ::arena0_primitives::commit_reveal::Message<#arg> })
}

fn primitive_type_name(ty: &syn::Type) -> String {
    if let syn::Type::Path(path) = ty
        && let Some(segment) = path.path.segments.last()
    {
        return segment.ident.to_string();
    }
    quote! { #ty }.to_string()
}

pub(crate) struct Arena0StateArgs {
    max: Option<syn::Expr>,
}

impl Parse for Arena0StateArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut max = None;

        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "max" => max = Some(input.parse()?),
                _ => return Err(Error::new(key.span(), "unsupported arena0::state argument")),
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(Self { max })
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn expand_arena0_state(
    args: Arena0StateArgs,
    input: DeriveInput,
) -> Result<TokenStream2> {
    struct FieldInfo {
        ident: syn::Ident,
        ty: syn::Type,
        clean_attrs: Vec<syn::Attribute>,
        is_primitive: bool,
        primitive_route: Option<syn::Path>,
    }

    let max = args
        .max
        .ok_or_else(|| Error::new(Span::call_site(), "arena0::state requires max = N"))?;

    let ident = &input.ident;
    let vis = &input.vis;

    crate::schema::validate_contract_attrs(&input.attrs)?;

    let Data::Struct(data) = &input.data else {
        return Err(Error::new(
            ident.span(),
            "arena0::state only supports structs",
        ));
    };

    let fields = match &data.fields {
        Fields::Named(fields) => &fields.named,
        _ => {
            return Err(Error::new(
                ident.span(),
                "arena0::state requires named fields",
            ));
        }
    };

    let mut parsed_fields = Vec::new();
    let mut phase_field = None;
    let mut phase_ty = None;

    for field in fields {
        let field_ident = field.ident.as_ref().unwrap();
        let field_ty = &field.ty;

        crate::schema::validate_contract_attrs(&field.attrs)?;

        let has_phase = field.attrs.iter().any(|a| a.path().is_ident("phase"));
        let has_secret = field.attrs.iter().any(|a| a.path().is_ident("secret"));
        let (has_primitive, primitive_route) = parse_primitive_attr(field)?;

        if has_secret || field.attrs.iter().any(|a| a.path().is_ident("private")) {
            return Err(Error::new(
                field_ident.span(),
                "inline #[private]/#[secret] fields are unsupported; move participant-local state to Program::Local",
            ));
        }

        // Strip custom annotations from the re-emitted struct.
        let clean_attrs: Vec<_> = field
            .attrs
            .iter()
            .filter(|a| {
                !a.path().is_ident("phase")
                    && !a.path().is_ident("primitive")
                    && !a.path().is_ident("secret")
                    && !a.path().is_ident("private")
            })
            .cloned()
            .collect();

        if has_phase {
            phase_field = Some(field_ident.clone());
            phase_ty = Some(field_ty.clone());
        }
        if has_primitive {
            if has_phase {
                return Err(Error::new(
                    field_ident.span(),
                    "#[primitive] field cannot be #[phase]",
                ));
            }
            if RESERVED_PRIMITIVE_ACCESSORS.contains(&field_ident.to_string().as_str()) {
                return Err(Error::new(
                    field_ident.span(),
                    format!(
                        "#[primitive] field name `{field_ident}` conflicts with an arena0 Context method; rename the field"
                    ),
                ));
            }
        }

        parsed_fields.push(FieldInfo {
            ident: field_ident.clone(),
            ty: field_ty.clone(),
            clean_attrs,
            is_primitive: has_primitive,
            primitive_route,
        });
    }

    // Fall back to a field named "phase" among shared-visible fields.
    if phase_field.is_none() {
        for f in &parsed_fields {
            if f.ident == "phase" {
                phase_field = Some(f.ident.clone());
                phase_ty = Some(f.ty.clone());
                break;
            }
        }
    }

    // Build the shared DTO's field definitions. The phase marker is replaced
    // with a runtime-managed wrapper; all other fields remain stock DTO fields.
    let struct_field_defs: Vec<_> = parsed_fields
        .iter()
        .map(|f| {
            let name = &f.ident;
            let ty = &f.ty;
            let attrs = &f.clean_attrs;
            if phase_field
                .as_ref()
                .is_some_and(|phase_field| name == phase_field)
            {
                let phase_ty = phase_ty.as_ref().expect("phase field has a type");
                quote! { #(#attrs)* pub #name: ::arena0::ManagedPhase<#phase_ty> }
            } else {
                quote! { #(#attrs)* pub #name: #ty }
            }
        })
        .collect();

    let program_value_field_types: Vec<_> = parsed_fields
        .iter()
        .map(|f| {
            if phase_field
                .as_ref()
                .is_some_and(|phase_field| f.ident == *phase_field)
            {
                let phase_ty = phase_ty.as_ref().expect("phase field has a type");
                syn::parse_quote!(::arena0::ManagedPhase<#phase_ty>)
            } else {
                f.ty.clone()
            }
        })
        .collect();
    let program_value_impl =
        crate::schema::expand_program_value_impl(ident, &input.generics, program_value_field_types);

    let required_capability_fields: Vec<_> = parsed_fields
        .iter()
        .filter(|field| field.is_primitive)
        .map(|field| {
            let ty = &field.ty;
            quote! {
                capabilities.extend(<#ty as ::arena0::Primitive>::required_capabilities());
            }
        })
        .collect();
    let mut primitive_output_counts: BTreeMap<String, Vec<&syn::Ident>> = BTreeMap::new();
    for f in parsed_fields.iter().filter(|f| f.is_primitive) {
        let Some(output_ty) = primitive_output_type(&f.ty) else {
            continue;
        };
        primitive_output_counts
            .entry(output_ty.to_string())
            .or_default()
            .push(&f.ident);
    }
    for f in parsed_fields
        .iter()
        .filter(|f| f.is_primitive && f.primitive_route.is_none())
    {
        let Some(output_ty) = primitive_output_type(&f.ty) else {
            continue;
        };
        let peers = primitive_output_counts
            .get(&output_ty.to_string())
            .map(Vec::as_slice)
            .unwrap_or_default();
        if peers.len() > 1 {
            let fields = peers
                .iter()
                .map(|ident| ident.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::new(
                f.ident.span(),
                format!(
                    "ambiguous primitive routing for `{}` fields: {fields}; add #[primitive(route = Message::...)] to each field",
                    primitive_type_name(&f.ty)
                ),
            ));
        }
    }
    let shared_impl = phase_field.as_ref().map(|phase_field| {
        let phase_ty = phase_ty.as_ref().expect("phase field has a type");
        quote! {
            impl ::arena0::PhasedSharedState for #ident {
                type Phase = #phase_ty;

                fn phase(&self) -> Self::Phase {
                    self.#phase_field.get()
                }

                fn __set_phase(&mut self, phase: Self::Phase) {
                    self.#phase_field = ::arena0::ManagedPhase::__new(phase);
                }
            }
        }
    });
    let accessor_trait = format_ident!("__Arena0{}PrimitiveAccess", ident);
    let shared_accessor_trait = format_ident!("__Arena0{}SharedPrimitiveAccess", ident);
    let generic_lookup_trait = format_ident!("__Arena0{}PrimitiveLookup", ident);
    let generic_has_trait = format_ident!("__Arena0{}HasPrimitive", ident);
    let shared_generic_lookup_trait = format_ident!("__Arena0{}SharedPrimitiveLookup", ident);
    let shared_generic_has_trait = format_ident!("__Arena0{}HasSharedPrimitive", ident);
    let primitive_routes: Vec<_> = parsed_fields
        .iter()
        .filter(|f| f.is_primitive)
        .filter_map(|f| {
            let route = f.primitive_route.as_ref()?;
            let route_ident = primitive_route_ident(ident, &f.ident);
            let output_ty = primitive_output_type(&f.ty).ok_or_else(|| {
                Error::new(
                    f.ident.span(),
                    "#[primitive(route = ...)] currently supports CommitReveal<T> and FairRandom fields",
                )
            });
            Some(output_ty.map(|output_ty| {
                quote! {
                    #[doc(hidden)]
                    pub struct #route_ident;

                    impl ::arena0::PrimitiveRoute<#output_ty> for #route_ident {
                        type Message = Message;

                        fn wrap(message: #output_ty) -> Self::Message {
                            #route(message)
                        }
                    }
                }
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let primitive_route_metadata: Vec<_> = parsed_fields
        .iter()
        .filter(|f| f.is_primitive)
        .map(|f| {
            let field_name = f.ident.to_string();
            let primitive_name = primitive_type_name(&f.ty);
            let message = f
                .primitive_route
                .as_ref()
                .map(|route| quote! { #route }.to_string())
                .unwrap_or_else(|| "raw".to_string());
            quote! {
                ::arena0::PrimitiveRouteSchema {
                    field: #field_name.into(),
                    primitive: #primitive_name.into(),
                    message: #message.into(),
                }
            }
        })
        .collect();

    let accessor_methods: Vec<_> = parsed_fields
        .iter()
        .filter(|f| f.is_primitive)
        .map(|f| {
            let name = &f.ident;
            let ty = &f.ty;
            if f.primitive_route.is_some() {
                let route_ident = primitive_route_ident(ident, name);
                let doc = format!("Access the `{name}` primitive field with generated routing metadata.");
                quote! {
                    #[doc = #doc]
                    fn #name(&mut self) -> ::arena0::LocalPrimitiveField<'_, #ident, __Arena0Local, #ty, #route_ident>;
                }
            } else {
                let doc = format!("Access the `{name}` primitive field.");
                quote! {
                    #[doc = #doc]
                    fn #name(&mut self) -> ::arena0::LocalPrimitiveField<'_, #ident, __Arena0Local, #ty>;
                }
            }
        })
        .collect();
    let accessor_impls: Vec<_> = parsed_fields
        .iter()
        .filter(|f| f.is_primitive)
        .map(|f| {
            let name = &f.ident;
            let ty = &f.ty;
            if f.primitive_route.is_some() {
                let route_ident = primitive_route_ident(ident, name);
                quote! {
                    fn #name(&mut self) -> ::arena0::LocalPrimitiveField<'_, #ident, __Arena0Local, #ty, #route_ident> {
                        fn field(state: &#ident) -> &#ty {
                            &state.#name
                        }
                        self.__local_primitive_field_routed(field)
                    }
                }
            } else {
                quote! {
                    fn #name(&mut self) -> ::arena0::LocalPrimitiveField<'_, #ident, __Arena0Local, #ty> {
                        fn field(state: &#ident) -> &#ty {
                            &state.#name
                        }
                        self.__local_primitive_field(field)
                    }
                }
            }
        })
        .collect();
    let shared_accessor_methods: Vec<_> = parsed_fields
        .iter()
        .filter(|f| f.is_primitive)
        .map(|f| {
            let name = &f.ident;
            let ty = &f.ty;
            if f.primitive_route.is_some() {
                let route_ident = primitive_route_ident(ident, name);
                let doc = format!("Access the `{name}` primitive field for a shared apply.");
                quote! {
                    #[doc = #doc]
                    fn #name(&mut self) -> ::arena0::SharedPrimitiveField<'_, #ident, #ty, #route_ident>;
                }
            } else {
                let doc = format!("Access the `{name}` primitive field for a shared apply.");
                quote! {
                    #[doc = #doc]
                    fn #name(&mut self) -> ::arena0::SharedPrimitiveField<'_, #ident, #ty>;
                }
            }
        })
        .collect();
    let shared_accessor_impls: Vec<_> = parsed_fields
        .iter()
        .filter(|f| f.is_primitive)
        .map(|f| {
            let name = &f.ident;
            let ty = &f.ty;
            if f.primitive_route.is_some() {
                let route_ident = primitive_route_ident(ident, name);
                quote! {
                    fn #name(&mut self) -> ::arena0::SharedPrimitiveField<'_, #ident, #ty, #route_ident> {
                        fn field(state: &mut #ident) -> &mut #ty {
                            &mut state.#name
                        }
                        self.__shared_primitive_field_routed(field)
                    }
                }
            } else {
                quote! {
                    fn #name(&mut self) -> ::arena0::SharedPrimitiveField<'_, #ident, #ty> {
                        fn field(state: &mut #ident) -> &mut #ty {
                            &mut state.#name
                        }
                        self.__shared_primitive_field(field)
                    }
                }
            }
        })
        .collect();
    let mut primitive_type_counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in parsed_fields.iter().filter(|f| f.is_primitive) {
        let ty = &f.ty;
        *primitive_type_counts
            .entry(quote! { #ty }.to_string())
            .or_default() += 1;
    }
    let generic_primitive_impls: Vec<_> = parsed_fields
        .iter()
        .filter(|f| f.is_primitive)
        .filter(|f| {
            let ty = &f.ty;
            primitive_type_counts.get(&quote! { #ty }.to_string()) == Some(&1)
        })
        .map(|f| {
            let name = &f.ident;
            let ty = &f.ty;
            let route_ty = if f.primitive_route.is_some() {
                let route_ident = primitive_route_ident(ident, name);
                quote! { #route_ident }
            } else {
                quote! { ::arena0::RawPrimitiveRoute }
            };
            let local_field_fn = format_ident!("__arena0_local_primitive_field_{}", name);
            let shared_field_fn = format_ident!("__arena0_shared_primitive_field_{}", name);
            let local_builder = if f.primitive_route.is_some() {
                quote! { self.__local_primitive_field_routed(#local_field_fn) }
            } else {
                quote! { self.__local_primitive_field(#local_field_fn) }
            };
            let shared_builder = if f.primitive_route.is_some() {
                quote! { self.__shared_primitive_field_routed(#shared_field_fn) }
            } else {
                quote! { self.__shared_primitive_field(#shared_field_fn) }
            };
            let context_impl = quote! {
                impl<__Arena0Local> #generic_has_trait<__Arena0Local, #ty>
                    for ::arena0::Context<#ident, __Arena0Local>
                {
                    type Access<'a> = ::arena0::LocalPrimitiveField<'a, #ident, __Arena0Local, #ty, #route_ty>
                    where
                        Self: 'a;

                    fn __arena0_primitive(&mut self) -> Self::Access<'_> {
                        fn #local_field_fn(state: &#ident) -> &#ty {
                            &state.#name
                        }
                        #local_builder
                    }
                }
            };
            let shared_impl = quote! {
                impl #shared_generic_has_trait<#ty>
                    for ::arena0::SharedContext<#ident>
                {
                    type Access<'a> = ::arena0::SharedPrimitiveField<'a, #ident, #ty, #route_ty>
                    where
                        Self: 'a;

                    fn __arena0_primitive(&mut self) -> Self::Access<'_> {
                        fn #shared_field_fn(state: &mut #ident) -> &mut #ty {
                            &mut state.#name
                        }
                        #shared_builder
                    }
                }
            };
            quote! {
                #context_impl
                #shared_impl
            }
        })
        .collect();

    Ok(quote! {
        #[derive(
            ::core::default::Default,
            ::core::fmt::Debug,
            ::arena0::serde::Serialize,
            ::arena0::serde::Deserialize,
            ::arena0::borsh::BorshSerialize,
            ::arena0::borsh::BorshDeserialize,
            ::arena0::schemars::JsonSchema,
        )]
        #[schemars(crate = "::arena0::schemars")]
        #vis struct #ident {
            #(#struct_field_defs,)*
        }

        #(#primitive_routes)*

        impl ::arena0::SharedState for #ident {
            const STATE_MAX: usize = #max;

            fn __primitive_routes() -> ::std::vec::Vec<::arena0::PrimitiveRouteSchema> {
                ::std::vec![#(#primitive_route_metadata),*]
            }

            fn __required_capabilities() -> ::std::vec::Vec<::arena0::Capability> {
                let mut capabilities = ::arena0::CapabilitySet::new();
                #(#required_capability_fields)*
                capabilities.into_vec()
            }
        }

        #shared_impl

        impl ::arena0::Primitive for #ident {
            fn required_capabilities() -> ::std::vec::Vec<::arena0::Capability> {
                let mut capabilities = ::arena0::CapabilitySet::new();
                #(#required_capability_fields)*
                capabilities.into_vec()
            }
        }

        #program_value_impl

        #[doc(hidden)]
        trait #accessor_trait<__Arena0Local> {
            #(#accessor_methods)*
        }

        #[doc(hidden)]
        trait #shared_accessor_trait {
            #(#shared_accessor_methods)*
        }

        #[doc(hidden)]
        trait #generic_has_trait<__Arena0Local, __Arena0Primitive> {
            type Access<'a>
            where
                Self: 'a;

            fn __arena0_primitive(&mut self) -> Self::Access<'_>;
        }

        #[doc(hidden)]
        trait #generic_lookup_trait<__Arena0Local> {
            fn primitive<__Arena0Primitive>(
                &mut self,
            ) -> <Self as #generic_has_trait<__Arena0Local, __Arena0Primitive>>::Access<'_>
            where
                Self: #generic_has_trait<__Arena0Local, __Arena0Primitive>;
        }

        #[doc(hidden)]
        trait #shared_generic_has_trait<__Arena0Primitive> {
            type Access<'a>
            where
                Self: 'a;

            fn __arena0_primitive(&mut self) -> Self::Access<'_>;
        }

        #[doc(hidden)]
        trait #shared_generic_lookup_trait {
            fn primitive<__Arena0Primitive>(
                &mut self,
            ) -> <Self as #shared_generic_has_trait<__Arena0Primitive>>::Access<'_>
            where
                Self: #shared_generic_has_trait<__Arena0Primitive>;
        }

        impl<__Arena0Local> #accessor_trait<__Arena0Local> for ::arena0::Context<#ident, __Arena0Local> {
            #(#accessor_impls)*
        }

        impl #shared_accessor_trait for ::arena0::SharedContext<#ident> {
            #(#shared_accessor_impls)*
        }

        impl<__Arena0Local> #generic_lookup_trait<__Arena0Local> for ::arena0::Context<#ident, __Arena0Local> {
            fn primitive<__Arena0Primitive>(
                &mut self,
            ) -> <Self as #generic_has_trait<__Arena0Local, __Arena0Primitive>>::Access<'_>
            where
                Self: #generic_has_trait<__Arena0Local, __Arena0Primitive>,
            {
                <Self as #generic_has_trait<__Arena0Local, __Arena0Primitive>>::__arena0_primitive(self)
            }
        }

        impl #shared_generic_lookup_trait for ::arena0::SharedContext<#ident> {
            fn primitive<__Arena0Primitive>(
                &mut self,
            ) -> <Self as #shared_generic_has_trait<__Arena0Primitive>>::Access<'_>
            where
                Self: #shared_generic_has_trait<__Arena0Primitive>,
            {
                <Self as #shared_generic_has_trait<__Arena0Primitive>>::__arena0_primitive(self)
            }
        }

        #(#generic_primitive_impls)*
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_fields_cannot_shadow_context_methods() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                phase: Phase,
                #[primitive]
                effects: CommitReveal,
            }
        };

        let err = expand_arena0_state(args, input).unwrap_err();
        assert!(
            err.to_string()
                .contains("conflicts with an arena0 Context method")
        );
    }

    #[test]
    fn primitive_route_generates_routed_accessor() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                phase: Phase,
                #[primitive(route = Message::CommitReveal)]
                commit_reveal: CommitReveal<Choice>,
            }
        };

        let expanded = expand_arena0_state(args, input).unwrap().to_string();
        assert!(expanded.contains("PrimitiveRoute"));
        assert!(expanded.contains("PrimitiveRouteSchema"));
        assert!(expanded.contains("Access the `commit_reveal` primitive field"));
        assert!(expanded.contains("\"commit_reveal\""));
        assert!(expanded.contains("\"Message :: CommitReveal\""));
        assert!(expanded.contains("Message"));
        assert!(expanded.contains("__local_primitive_field_routed"));
        assert!(expanded.contains("__shared_primitive_field_routed"));
    }

    #[test]
    fn unique_primitive_type_generates_generic_lookup() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                phase: Phase,
                #[primitive(route = Message::CommitReveal)]
                commit_reveal: CommitReveal<Choice>,
            }
        };

        let expanded = expand_arena0_state(args, input).unwrap().to_string();
        assert!(expanded.contains("HasPrimitive"));
        assert!(expanded.contains("CommitReveal < Choice >"));
        assert!(expanded.contains("type Access"));
        assert!(expanded.contains("__local_primitive_field_routed"));
        assert!(expanded.contains("__shared_primitive_field_routed"));
    }

    #[test]
    fn duplicate_primitive_type_keeps_generic_lookup_ambiguous() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                phase: Phase,
                #[primitive(route = Message::Decision)]
                decision: CommitReveal<Choice>,
                #[primitive(route = Message::Tiebreak)]
                tiebreak: CommitReveal<Choice>,
            }
        };

        let expanded = expand_arena0_state(args, input).unwrap().to_string();
        assert!(expanded.contains("fn decision"));
        assert!(expanded.contains("fn tiebreak"));
        assert!(!expanded.contains("HasPrimitive < __Arena0Local , CommitReveal < Choice > >"));
    }

    #[test]
    fn duplicate_unrouted_primitives_reject_ambiguous_routing() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                phase: Phase,
                #[primitive]
                decision: CommitReveal<Choice>,
                #[primitive]
                tiebreak: CommitReveal<Choice>,
            }
        };

        let err = expand_arena0_state(args, input).unwrap_err().to_string();
        assert!(err.contains("ambiguous primitive routing"));
        assert!(err.contains("decision, tiebreak"));
        assert!(err.contains("#[primitive(route = Message::...)]"));
    }

    #[test]
    fn managed_phase_field_uses_managed_phase_wrapper() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                #[phase]
                phase: Phase,
            }
        };

        let expanded = expand_arena0_state(args, input).unwrap().to_string();
        assert!(expanded.contains("ManagedPhase < Phase >"));
        assert!(!expanded.contains("pub phase : Phase"));
    }

    #[test]
    fn state_derives_debug_for_default_view() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                round: u64,
            }
        };

        let expanded = expand_arena0_state(args, input).unwrap().to_string();

        assert!(expanded.contains(":: core :: fmt :: Debug"));
    }

    #[test]
    fn inline_private_fields_are_rejected() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                visible: u32,
                #[private]
                secret: u64,
            }
        };

        let error = expand_arena0_state(args, input).unwrap_err();
        assert!(error.to_string().contains("move participant-local state"));
    }

    #[test]
    fn secret_shared_fields_are_rejected() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                phase: Phase,
                #[secret]
                salt: [u8; 32],
            }
        };

        let err = expand_arena0_state(args, input).unwrap_err();
        assert!(err.to_string().contains("move participant-local state"));
    }

    #[test]
    fn state_uses_unconditional_stock_borsh() {
        let args = Arena0StateArgs {
            max: Some(syn::parse_quote!(256)),
        };
        let input: DeriveInput = syn::parse_quote! {
            pub struct Shared {
                value: u32,
            }
        };

        let expanded = expand_arena0_state(args, input).unwrap().to_string();
        assert!(expanded.contains("BorshSerialize"));
        assert!(expanded.contains("BorshDeserialize"));
        assert!(!expanded.contains("shared_write"));
        assert!(!expanded.contains("shared_read"));
        assert!(!expanded.contains("__shared"));
    }
}
