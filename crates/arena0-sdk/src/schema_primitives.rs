//! `ProgramValue` implementations for the closed set of public ABI values.

use arena0_protocol::{Participant, PeerId};

use crate::ProgramValue;

// Keep this list explicit. Upstream `JsonSchema`
// implementations must not make a new Rust type public by accident.
macro_rules! impl_program_value {
    ($($ty:ty),+ $(,)?) => {
        $(impl ProgramValue for $ty {})+
    };
}

impl_program_value!(
    bool,
    u8,
    u16,
    u32,
    u64,
    i8,
    i16,
    i32,
    i64,
    String,
    (),
    PeerId,
    Participant,
);

impl<T: ProgramValue> ProgramValue for Option<T> {}
impl<T: ProgramValue> ProgramValue for Vec<T> {}
impl<T: ProgramValue, const N: usize> ProgramValue for [T; N] where [T; N]: schemars::JsonSchema {}

macro_rules! impl_program_value_tuple {
    ($($name:ident),+) => {
        impl<$($name: ProgramValue),+> ProgramValue for ($($name,)+) {}
    };
}

// Schemars 1.2 provides tuple schemas through arity 16.
impl_program_value_tuple!(A);
impl_program_value_tuple!(A, B);
impl_program_value_tuple!(A, B, C);
impl_program_value_tuple!(A, B, C, D);
impl_program_value_tuple!(A, B, C, D, E);
impl_program_value_tuple!(A, B, C, D, E, F);
impl_program_value_tuple!(A, B, C, D, E, F, G);
impl_program_value_tuple!(A, B, C, D, E, F, G, H);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I, J, K, L, M);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I, J, K, L, M, N);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O);
impl_program_value_tuple!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arena0Callout;

    #[allow(dead_code)]
    #[arena0::primitive]
    #[derive(Default)]
    struct SchemaFixture {
        first: u32,
        second: String,
    }

    #[allow(dead_code)]
    #[arena0::state(max = 256)]
    struct StateFixture {
        first: u32,
        second: String,
    }

    #[arena0::phases]
    enum TestPhase {
        #[phase(default)]
        Start,
    }

    #[arena0::data]
    struct DataFixture {
        number: u32,
        text: Option<String>,
    }

    #[arena0::callouts]
    enum CalloutFixture {
        Choose { prompt: String },
    }

    #[arena0::query]
    enum QueryFixture {
        #[query(response = u32)]
        Count(u32),
    }

    #[allow(dead_code)]
    #[arena0::state(max = 256)]
    struct PhasedStateFixture {
        #[phase]
        phase: TestPhase,
        visible: u32,
    }

    #[test]
    fn primitive_schema_and_borsh_are_stock_contracts() {
        let fixture = SchemaFixture {
            first: 7,
            second: "value".into(),
        };
        let encoded = borsh::to_vec(&fixture).expect("primitive Borsh encoding");
        let decoded: SchemaFixture = borsh::from_slice(&encoded).expect("primitive Borsh decoding");
        assert_eq!(decoded.first, fixture.first);
        assert_eq!(decoded.second, fixture.second);

        let schema = SchemaFixture::json_schema();
        assert_eq!(
            schema.as_value()["$schema"],
            schemars::consts::meta_schemas::DRAFT2020_12
        );

        let properties = schema.as_value()["properties"]
            .as_object()
            .expect("object schema properties");
        assert!(properties.contains_key("first"));
        assert!(properties.contains_key("second"));
    }

    fn assert_program_value<T: ProgramValue>() {}

    #[test]
    fn allowlisted_composites_implement_program_value() {
        assert_program_value::<Option<Vec<(u32, [u8; 2])>>>();
        assert_program_value::<PeerId>();
        assert_program_value::<Participant>();
    }

    #[test]
    fn state_schema_and_borsh_cover_shared_fields() {
        let fixture = StateFixture {
            first: 7,
            second: "value".into(),
        };
        let encoded = borsh::to_vec(&fixture).expect("state Borsh encoding");
        let decoded: StateFixture = borsh::from_slice(&encoded).expect("state Borsh decoding");
        assert_eq!(decoded.first, fixture.first);
        assert_eq!(decoded.second, fixture.second);

        let schema = StateFixture::json_schema();
        let properties = schema.as_value()["properties"]
            .as_object()
            .expect("object schema properties");
        assert!(properties.contains_key("first"));
        assert!(properties.contains_key("second"));
    }

    #[test]
    fn managed_phase_schema_uses_the_phase_shape() {
        let schema = PhasedStateFixture::json_schema();
        let properties = schema.as_value()["properties"]
            .as_object()
            .expect("object schema properties");
        assert!(properties.contains_key("phase"));
        assert!(properties.contains_key("visible"));
        assert!(properties["phase"]["$ref"].is_string());
    }

    #[test]
    fn authoring_macros_generate_json_schemas() {
        let data = DataFixture::json_schema();
        let callout = CalloutFixture::json_schema();
        let query = QueryFixture::json_schema();
        for schema in [&data, &callout, &query] {
            assert_eq!(
                schema.as_value()["$schema"],
                schemars::consts::meta_schemas::DRAFT2020_12
            );
        }

        let query_schemas = <QueryFixture as crate::Arena0Query>::schemas();
        assert_eq!(query_schemas.len(), 1);
        assert_eq!(query_schemas[0].name, "QueryFixture");
        assert!(query_schemas[0].request.as_value()["oneOf"].is_array());
        assert!(query_schemas[0].response.as_value()["oneOf"].is_array());
    }

    #[test]
    fn callout_answers_cross_as_json() {
        let value = String::from("answer");
        let bytes = serde_json::to_vec(&value).expect("string JSON encoding");
        let decoded: String = crate::__parse_input_data(&bytes);
        assert_eq!(decoded, value);
        assert_eq!(crate::__serialize_input_data(&value), bytes);

        let input = CalloutFixture::from_raw(0, bytes.clone());
        let CalloutFixtureInput::Choose(answer) = input;
        assert_eq!(answer, value);

        let encoded = CalloutFixture::to_event_data(&CalloutFixtureInput::Choose(value));
        assert_eq!(encoded, (0, bytes));
    }
}
