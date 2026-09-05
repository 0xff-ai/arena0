//! Proc macros for authoring arena0 wasm programs.
//!
//! We provide attribute macros (`#[arena0::program]`, `#[arena0::state]`, `#[arena0::primitive]`,
//! `#[arena0::data]`, `#[arena0::outcome]`, `#[arena0::message]`, `#[arena0::local]`,
//! `#[arena0::phases]`, `#[arena0::pending]`, `#[arena0::callouts]`, `#[arena0::callout]`,
//! `#[arena0::query]`). Together these generate the wasm ABI glue, metadata
//! exports, state serialization, and typed callout/query enums that the arena0
//! runtime expects. Programs re-export these through `arena0`; direct use is
//! uncommon.

mod arena0_test;
mod attr_callout;
mod attr_local;
mod attr_pending;
mod attr_phases;
mod attr_primitive;
mod attr_query;
mod attr_state;
mod attr_type;
mod program;
mod schema;
mod util;

use proc_macro::TokenStream;
use syn::{DeriveInput, Item, ItemEnum, parse_macro_input};

/// Derives the standard traits and paired `ProgramValue` contract for a
/// program-visible type.
#[proc_macro_attribute]
pub fn data(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as attr_type::ProgramValueArgs);
    match attr_type::expand_program_value(args, parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Derives the standard traits and paired `ProgramValue` contract for a
/// message type.
#[proc_macro_attribute]
pub fn message(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as attr_type::ProgramValueArgs);
    match attr_type::expand_arena0_message(args, parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Derives the standard traits and paired `ProgramValue` contract for a
/// program's derived `Outcome` type. The `#[arena0::program]` macro emits the
/// contract into `arena0_metadata` and the agent-facing JSON `arena0_outcome`
/// export from this type.
#[proc_macro_attribute]
pub fn outcome(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as attr_type::ProgramValueArgs);
    match attr_type::expand_program_value(args, parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates redacted `Debug` and local debug snapshot support for participant-local state.
#[proc_macro_attribute]
pub fn local(_args: TokenStream, input: TokenStream) -> TokenStream {
    match attr_local::expand_arena0_local(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates `Arena0Phase`, `ProgramValue`, and `Default` impls from a phase
/// enum.
#[proc_macro_attribute]
pub fn phases(_args: TokenStream, input: TokenStream) -> TokenStream {
    match attr_phases::expand_arena0_phases(parse_macro_input!(input as ItemEnum)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates local pending-label metadata, `Display`, and `ProgramValue` from
/// a unit enum.
#[proc_macro_attribute]
pub fn pending(_args: TokenStream, input: TokenStream) -> TokenStream {
    match attr_pending::expand_arena0_pending(parse_macro_input!(input as ItemEnum)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates typed `Callout` and `Input` enums with `Arena0Callout` glue.
#[proc_macro_attribute]
pub fn callouts(_args: TokenStream, input: TokenStream) -> TokenStream {
    match attr_callout::expand_arena0_callouts(parse_macro_input!(input as ItemEnum)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Variant-level attribute for callout configuration (parsed by `callouts`).
#[proc_macro_attribute]
pub fn callout(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Generates typed `Query` and `QueryResponse` enums with `Arena0Query` glue from a declaration enum.
#[proc_macro_attribute]
pub fn query(_args: TokenStream, input: TokenStream) -> TokenStream {
    match attr_query::expand_arena0_query(parse_macro_input!(input as ItemEnum)) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates stock Borsh/Serde, `Debug`, `Clone`, `Primitive`, and
/// `ProgramValue` impls from a struct. Primitive-owned capabilities can be
/// declared with `capabilities(...)`.
/// Inline `#[private]` and `#[secret]` fields are rejected; local semantic
/// state belongs in `Program::Local`.
#[proc_macro_attribute]
pub fn primitive(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as attr_primitive::PrimitiveArgs);
    let input = parse_macro_input!(input as DeriveInput);
    match attr_primitive::expand(args, input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates a flat shared state struct with stock Borsh serialization.
/// All fields are shared-visible; local semantic state belongs in `Program::Local`.
/// `#[phase]` identifies the lifecycle phase field (falls back to a field named `phase`).
/// `#[primitive]` marks a shared field that should receive a generated context accessor.
#[proc_macro_attribute]
pub fn state(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as attr_state::Arena0StateArgs);
    let input = parse_macro_input!(input as DeriveInput);
    match attr_state::expand_arena0_state(args, input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates wasm ABI exports, state storage, and metadata for a `Program` trait impl or module shell.
#[proc_macro_attribute]
pub fn program(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as program::Arena0ProgramArgs);
    let item = parse_macro_input!(input as Item);
    match program::expand_arena0_program_item(args, item) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Generates dual test functions (native + sandbox) from a single test definition.
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    arena0_test::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
