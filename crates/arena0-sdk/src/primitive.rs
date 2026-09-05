use borsh::{BorshDeserialize, BorshSerialize};

/// A composable shared-state value.
///
/// `Primitive` is a capability declaration layered on top of the stock Borsh
/// contract. It does not define another serializer: a primitive's exact
/// representation is always its ordinary [`BorshSerialize`] and
/// [`BorshDeserialize`] implementation. Derive it with
/// `#[arena0::primitive]` for reusable protocol state.
pub trait Primitive: Default + BorshSerialize + BorshDeserialize {
    /// Host capabilities required by this primitive's effects.
    ///
    /// Primitive implementations own this declaration. Program metadata merges
    /// it with capabilities declared by the program itself.
    fn required_capabilities() -> Vec<arena0_program::Capability> {
        Vec::new()
    }
}

impl<T> Primitive for Option<T>
where
    T: Primitive,
{
    fn required_capabilities() -> Vec<arena0_program::Capability> {
        T::required_capabilities()
    }
}
