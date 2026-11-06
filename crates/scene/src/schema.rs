//! The model-facing 2D JSON contract; runtime admission remains authoritative.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

use crate::id::Ms;
use crate::limits::{
    MAX_COORD, MAX_ID_CHARS, MAX_MOTION_MS, MAX_SOURCE_BYTES, MAX_STEPS, MAX_TEXT_CHARS,
};
use crate::record::{Anchor, Cell, Color, KeyValue, Point, Start, Token, Version};
use crate::text::{Bounded, Number};
use crate::{Finite, Record, Source, Text, WireId};

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
struct Envelope {
    #[expect(
        dead_code,
        reason = "only the generated envelope schema consumes this field"
    )]
    records: Bounded<Record, { MAX_STEPS + 2 }>,
}

/// JSON Schema for a complete 2D draw envelope, using scene's JSON field forms.
#[must_use]
pub fn draw_schema() -> serde_json::Value {
    schemars::schema_for!(Envelope).to_value()
}

macro_rules! schema {
    ($ty:ty, $generator:ident => $body:expr) => {
        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> {
                stringify!($ty).into()
            }

            fn json_schema($generator: &mut SchemaGenerator) -> Schema {
                $body
            }
        }
    };
}

schema!(Version, _generator => json_schema!({"const": 1}));
schema!(WireId, _generator => json_schema!({
    "type": "string", "minLength": 1, "maxLength": MAX_ID_CHARS,
    "pattern": "^[A-Za-z0-9_.-]+$"
}));
schema!(Text, _generator => json_schema!({
    "type": "string", "maxLength": MAX_TEXT_CHARS
}));
schema!(Source, _generator => json_schema!({
    "type": "string", "maxLength": MAX_SOURCE_BYTES,
    "description": "UTF-8 source also has a byte limit enforced by the decoder."
}));
schema!(Finite, _generator => json_schema!({
    "type": "number", "minimum": -MAX_COORD, "maximum": MAX_COORD
}));
schema!(Number, _generator => json_schema!({"type": "number"}));
schema!(Ms, _generator => json_schema!({
    "type": "integer", "minimum": 0, "maximum": MAX_MOTION_MS
}));
schema!(Start, generator => json_schema!({"anyOf": [
    {"enum": ["speech", "now"]},
    {"type": "object", "properties": {
        "after": generator.subschema_for::<WireId>(),
        "ms": generator.subschema_for::<Ms>()
    }, "required": ["after", "ms"], "additionalProperties": false}
]}));
schema!(Anchor, generator => json_schema!({"anyOf": [
    generator.subschema_for::<[Finite; 2]>(),
    {"type": "object", "properties": {"ref": generator.subschema_for::<WireId>()},
     "required": ["ref"], "additionalProperties": false}
]}));
schema!(Color, generator => json_schema!({"anyOf": [
    generator.subschema_for::<Token>(),
    {"type": "string", "pattern": "^#[0-9a-fA-F]{6}$"}
]}));
schema!(Cell, generator => json_schema!({"anyOf": [
    generator.subschema_for::<Text>(), generator.subschema_for::<Number>(),
    {"type": "boolean"}
]}));
schema!(Point, generator => json_schema!({"anyOf": [
    generator.subschema_for::<Number>(), generator.subschema_for::<[Number; 2]>()
]}));
schema!(KeyValue, generator => json_schema!({"anyOf": [
    generator.subschema_for::<Finite>(), generator.subschema_for::<[Finite; 2]>(),
    generator.subschema_for::<Color>()
]}));

impl<T: JsonSchema, const N: usize> JsonSchema for Bounded<T, N> {
    fn schema_name() -> Cow<'static, str> {
        format!("Bounded_{}_{N}", T::schema_name()).into()
    }

    fn schema_id() -> Cow<'static, str> {
        format!("Bounded<{}, {N}>", T::schema_id()).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "array", "items": generator.subschema_for::<T>(), "maxItems": N
        })
    }
}
