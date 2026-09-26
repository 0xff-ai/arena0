//! `#[arena0::data]` on a generic type honors explicit contract bounds.

use arena0::ProgramValue;

#[arena0::data(bound = "T: ::arena0::ProgramValue")]
#[serde(bound = "T: ::arena0::serde::Serialize + ::arena0::serde::de::DeserializeOwned")]
struct ExplicitBounds<T> {
    value: T,
}

#[test]
fn public_data_macro_compiles_explicit_generic_contract_bounds() {
    let value = ExplicitBounds {
        value: String::from("compiled"),
    };
    let encoded = arena0::borsh::to_vec(&value).expect("Borsh encoding");
    let decoded: ExplicitBounds<String> =
        arena0::borsh::from_slice(&encoded).expect("Borsh decoding");
    assert_eq!(decoded.value, value.value);
    assert!(
        ExplicitBounds::<String>::json_schema()
            .as_value()
            .is_object()
    );
}
