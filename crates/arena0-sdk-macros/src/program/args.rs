//! Argument parsing for `#[arena0::program]`.
//!
//! This is the parsing/model seam: it turns the macro's attribute tokens into
//! the typed configuration the expanders consume. Session admission belongs to
//! the host negotiation protocol, so the program attribute has no session
//! policy.

use proc_macro2::TokenStream as TokenStream2;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Error, Expr, ExprLit, ExprRange, Ident, Lit, LitInt, LitStr, RangeLimits, Result, Token,
};

#[derive(Clone)]
pub(crate) struct Arena0ProgramArgs {
    pub(super) name: LitStr,
    pub(super) version: LitStr,
    pub(super) description: LitStr,
    pub(super) display_name: Option<LitStr>,
    pub(super) participants: ParticipantCountArgs,
    pub(super) capabilities: TokenStream2,
    pub(super) capabilities_auto: bool,
}

/// `participants = 2` or `participants = 2..=64`.
#[derive(Clone)]
pub(super) enum ParticipantCountArgs {
    Exact(LitInt),
    Range { min: LitInt, max: LitInt },
}

impl Parse for ParticipantCountArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let expression: Expr = input.parse()?;
        match expression {
            Expr::Lit(ExprLit {
                lit: Lit::Int(count),
                ..
            }) => {
                count
                    .base10_parse::<u8>()
                    .map_err(|_| Error::new(count.span(), "participants must fit in a u8"))?;
                Ok(Self::Exact(count))
            }
            Expr::Range(ExprRange {
                start,
                limits: RangeLimits::Closed(_),
                end,
                ..
            }) => {
                let min = integer_literal(start, "participant range minimum")?;
                let max = integer_literal(end, "participant range maximum")?;
                let min_value = min
                    .base10_parse::<u8>()
                    .map_err(|_| Error::new(min.span(), "participant bounds must fit in a u8"))?;
                let max_value = max
                    .base10_parse::<u8>()
                    .map_err(|_| Error::new(max.span(), "participant bounds must fit in a u8"))?;
                if min_value > max_value {
                    return Err(Error::new(
                        min.span().join(max.span()).unwrap_or(min.span()),
                        "participant range minimum must not exceed its maximum",
                    ));
                }
                Ok(Self::Range { min, max })
            }
            Expr::Range(range) => Err(Error::new(
                range.span(),
                "participants range must use inclusive bounds, for example `2..=64`",
            )),
            other => Err(Error::new(
                other.span(),
                "participants must be an integer or inclusive range, for example `2..=64`",
            )),
        }
    }
}

impl ParticipantCountArgs {
    pub(super) fn to_tokens(&self) -> TokenStream2 {
        match self {
            Self::Exact(count) => quote::quote! {
                ::arena0::ParticipantCount::Exact { count: #count }
            },
            Self::Range { min, max } => quote::quote! {
                ::arena0::ParticipantCount::Range { min: #min, max: #max }
            },
        }
    }
}

fn integer_literal(expression: Option<Box<Expr>>, bound: &str) -> Result<LitInt> {
    let Some(expression) = expression else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!("{bound} must be an integer literal"),
        ));
    };
    match *expression {
        Expr::Lit(ExprLit {
            lit: Lit::Int(value),
            ..
        }) => Ok(value),
        other => Err(Error::new(
            other.span(),
            format!("{bound} must be an integer literal"),
        )),
    }
}

impl Parse for Arena0ProgramArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        let mut version = None;
        let mut description = None;
        let mut display_name = None;
        let mut participants = None;
        let mut capabilities = None;
        let mut capabilities_auto = false;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            match key.to_string().as_str() {
                "name" => {
                    input.parse::<Token![=]>()?;
                    name = Some(input.parse()?);
                }
                "version" => {
                    input.parse::<Token![=]>()?;
                    version = Some(input.parse()?);
                }
                "description" => {
                    input.parse::<Token![=]>()?;
                    description = Some(input.parse()?);
                }
                "display_name" => {
                    input.parse::<Token![=]>()?;
                    display_name = Some(input.parse()?);
                }
                "participants" => {
                    input.parse::<Token![=]>()?;
                    participants = Some(input.parse()?);
                }
                "capabilities" => {
                    let content;
                    syn::parenthesized!(content in input);
                    let tokens: TokenStream2 = content.parse()?;
                    let auto = token_stream_contains_ident(&tokens, "auto");
                    capabilities = Some(tokens);
                    capabilities_auto = capabilities_auto || auto;
                }
                _ => {
                    return Err(Error::new(
                        key.span(),
                        "unsupported arena0::program argument",
                    ));
                }
            }

            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(Self {
            name: name.ok_or_else(|| Error::new(proc_macro2::Span::call_site(), "missing name"))?,
            version: version
                .ok_or_else(|| Error::new(proc_macro2::Span::call_site(), "missing version"))?,
            description: description
                .ok_or_else(|| Error::new(proc_macro2::Span::call_site(), "missing description"))?,
            display_name,
            participants: participants.ok_or_else(|| {
                Error::new(proc_macro2::Span::call_site(), "missing participants")
            })?,
            capabilities: capabilities.unwrap_or_default(),
            capabilities_auto,
        })
    }
}
fn token_stream_contains_ident(tokens: &TokenStream2, ident: &str) -> bool {
    tokens.clone().into_iter().any(|tree| match tree {
        proc_macro2::TokenTree::Ident(found) => found == ident,
        proc_macro2::TokenTree::Group(group) => token_stream_contains_ident(&group.stream(), ident),
        _ => false,
    })
}
