use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{FnArg, ItemFn, Pat, Token, Type};

pub(crate) struct ArenaTestArgs {
    program_type: Type,
    params: syn::Expr,
}

impl Parse for ArenaTestArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let program_type: Type = input.parse()?;
        input.parse::<Token![,]>()?;
        let params: syn::Expr = input.parse()?;

        Ok(Self {
            program_type,
            params,
        })
    }
}

fn extract_param_name(sig: &syn::Signature) -> syn::Result<Ident> {
    let param = sig.inputs.first().ok_or_else(|| {
        syn::Error::new_spanned(
            sig,
            "arena0::test function must have one parameter (the harness binding)",
        )
    })?;

    match param {
        FnArg::Typed(pat_type) => match pat_type.pat.as_ref() {
            Pat::Ident(pat_ident) => Ok(pat_ident.ident.clone()),
            other => Err(syn::Error::new_spanned(
                other,
                "expected a simple identifier",
            )),
        },
        FnArg::Receiver(r) => Err(syn::Error::new_spanned(
            r,
            "arena0::test function cannot take self",
        )),
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args: ArenaTestArgs = syn::parse2(attr)?;
    let item_fn: ItemFn = syn::parse2(item)?;

    let fn_name = &item_fn.sig.ident;
    let body = &item_fn.block;
    let harness_ident = extract_param_name(&item_fn.sig)?;

    let native_name = format_ident!("{fn_name}__native");
    let program_ty = &args.program_type;
    let params = &args.params;
    let h = &harness_ident;

    let native_fn = quote! {
        #[test]
        fn #native_name() {
            let mut #h = ::arena0::testing::TestHarness::<#program_ty>::new(#params);
            #body
        }
    };

    Ok(native_fn)
}
