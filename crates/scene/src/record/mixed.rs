//! The syntax that differs between JSON and binary encodings.
use crate::{
    ErrorCode,
    id::{Ms, WireId},
    text::{Finite, Number, Text},
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer},
};
use std::fmt;

/// A validated 24-bit colour.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Rgb(u32);
impl fmt::Debug for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rgb(..)")
    }
}
impl Rgb {
    /// Returns the packed RGB value.
    pub fn get(self) -> u32 {
        self.0
    }
}
impl TryFrom<u32> for Rgb {
    type Error = ErrorCode;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value > 0x00ff_ffff {
            Err(ErrorCode::OutOfRange)
        } else {
            Ok(Self(value))
        }
    }
}
impl From<Rgb> for u32 {
    fn from(value: Rgb) -> Self {
        value.0
    }
}

/// A theme colour token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Token {
    /// Blue.
    Blue,
    /// Green.
    Green,
    /// Red.
    Red,
    /// Orange.
    Orange,
    /// Amber.
    Amber,
    /// Purple.
    Purple,
    /// Teal.
    Teal,
    /// Pink.
    Pink,
    /// Accent.
    Accent,
    /// Surface.
    Surface,
    /// Fg.
    Fg,
    /// Muted.
    Muted,
    /// Gray.
    Gray,
    /// White.
    White,
    /// Black.
    Black,
}

/// One step's activation dependency.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Start {
    /// Start with speech or the synthetic segment.
    #[default]
    Speech,
    /// Start immediately.
    Now,
    /// Start after an earlier step.
    After {
        /// Earlier step id.
        step: WireId,
        /// Delay in milliseconds.
        ms: Ms,
    },
}
#[derive(Serialize, Deserialize)]
enum StartBinary {
    Speech,
    Now,
    After { step: WireId, ms: Ms },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct After {
    after: WireId,
    ms: Ms,
}
impl Start {
    fn from_binary(value: StartBinary) -> Self {
        match value {
            StartBinary::Speech => Self::Speech,
            StartBinary::Now => Self::Now,
            StartBinary::After { step, ms } => Self::After { step, ms },
        }
    }
}
impl Serialize for Start {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            Self::Speech => StartBinary::Speech,
            Self::Now => StartBinary::Now,
            Self::After { step, ms } => StartBinary::After {
                step: step.clone(),
                ms: *ms,
            },
        };
        if !s.is_human_readable() {
            return value.serialize(s);
        }
        match value {
            StartBinary::Speech => s.serialize_str("speech"),
            StartBinary::Now => s.serialize_str("now"),
            StartBinary::After { step, ms } => After { after: step, ms }.serialize(s),
        }
    }
}
impl<'de> Deserialize<'de> for Start {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct StartVisitor;
        impl<'de> Visitor<'de> for StartVisitor {
            type Value = Start;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an activation dependency")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Start, E> {
                let value = match value {
                    "speech" => StartBinary::Speech,
                    "now" => StartBinary::Now,
                    _ => return Err(E::custom(ErrorCode::OutOfRange)),
                };
                Ok(Start::from_binary(value))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Start, A::Error> {
                let After { after, ms } = After::deserialize(MapAccessDeserializer::new(map))?;
                Ok(Start::from_binary(StartBinary::After { step: after, ms }))
            }
        }
        if !d.is_human_readable() {
            return StartBinary::deserialize(d).map(Self::from_binary);
        }
        d.deserialize_any(StartVisitor)
    }
}

/// A canvas point or an object reference.
#[derive(Debug, Clone, PartialEq)]
pub enum Anchor {
    /// Logical canvas coordinates.
    At([Finite; 2]),
    /// A scene object.
    Ref(WireId),
}
#[derive(Serialize, Deserialize)]
enum AnchorBinary {
    At([Finite; 2]),
    Ref(WireId),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    #[serde(rename = "ref")]
    id: WireId,
}
impl Anchor {
    fn from_binary(value: AnchorBinary) -> Self {
        match value {
            AnchorBinary::At(v) => Self::At(v),
            AnchorBinary::Ref(v) => Self::Ref(v),
        }
    }
}
impl Serialize for Anchor {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return match self {
                Self::At(v) => v.serialize(s),
                Self::Ref(v) => Reference { id: v.clone() }.serialize(s),
            };
        }
        match self {
            Self::At(v) => AnchorBinary::At(*v),
            Self::Ref(v) => AnchorBinary::Ref(v.clone()),
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for Anchor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct AnchorVisitor;
        impl<'de> Visitor<'de> for AnchorVisitor {
            type Value = Anchor;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a canvas point or reference")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Anchor, A::Error> {
                let point = <[Finite; 2]>::deserialize(de::value::SeqAccessDeserializer::new(seq))?;
                Ok(Anchor::from_binary(AnchorBinary::At(point)))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Anchor, A::Error> {
                let reference = Reference::deserialize(MapAccessDeserializer::new(map))?;
                Ok(Anchor::from_binary(AnchorBinary::Ref(reference.id)))
            }
        }
        if !d.is_human_readable() {
            return AnchorBinary::deserialize(d).map(Self::from_binary);
        }
        d.deserialize_any(AnchorVisitor)
    }
}

/// A palette token or checked RGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Theme palette entry.
    Token(Token),
    /// Literal RGB.
    Hex(Rgb),
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum ColorBinary {
    Token(Token),
    Hex(Rgb),
}
impl Color {
    fn from_binary(value: ColorBinary) -> Self {
        match value {
            ColorBinary::Token(v) => Self::Token(v),
            ColorBinary::Hex(v) => Self::Hex(v),
        }
    }
    fn from_text<E: de::Error>(value: &str) -> Result<Self, E> {
        if let Some(hex) = value.strip_prefix('#') {
            if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(E::custom(ErrorCode::OutOfRange));
            }
            let value =
                u32::from_str_radix(hex, 16).map_err(|_| E::custom(ErrorCode::OutOfRange))?;
            let value = Rgb::try_from(value).map_err(E::custom)?;
            return Ok(Self::from_binary(ColorBinary::Hex(value)));
        }
        Token::deserialize(de::value::StrDeserializer::<E>::new(value))
            .map(|token| Self::from_binary(ColorBinary::Token(token)))
    }
}
impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return match self {
                Self::Token(v) => v.serialize(s),
                Self::Hex(v) => s.serialize_str(&format!("#{:06x}", v.get())),
            };
        }
        match self {
            Self::Token(v) => ColorBinary::Token(*v),
            Self::Hex(v) => ColorBinary::Hex(*v),
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ColorVisitor;
        impl Visitor<'_> for ColorVisitor {
            type Value = Color;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a palette token or RGB colour")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Color, E> {
                Color::from_text(value)
            }
        }
        if !d.is_human_readable() {
            return ColorBinary::deserialize(d).map(Self::from_binary);
        }
        d.deserialize_str(ColorVisitor)
    }
}

/// Cell with the same validated payloads in JSON and binary.
#[derive(Clone, PartialEq)]
pub enum Cell {
    /// Text value.
    Text(Text),
    /// Number value.
    Number(Number),
    /// Bool value.
    Bool(bool),
}
impl fmt::Debug for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Cell(..)")
    }
}
#[derive(Serialize, Deserialize)]
enum CellBinary {
    Text(Text),
    Number(Number),
    Bool(bool),
}
impl Cell {
    fn from_binary(value: CellBinary) -> Self {
        match value {
            CellBinary::Text(v) => Self::Text(v),
            CellBinary::Number(v) => Self::Number(v),
            CellBinary::Bool(v) => Self::Bool(v),
        }
    }
}
impl Serialize for Cell {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return match self {
                Self::Text(v) => v.serialize(s),
                Self::Number(v) => v.serialize(s),
                Self::Bool(v) => v.serialize(s),
            };
        }
        match self {
            Self::Text(v) => CellBinary::Text(v.clone()),
            Self::Number(v) => CellBinary::Number(*v),
            Self::Bool(v) => CellBinary::Bool(*v),
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl Visitor<'_> for ValueVisitor {
            type Value = Cell;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a validated cell")
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Cell, E> {
                let number = Number::deserialize(de::value::F64Deserializer::<E>::new(value))?;
                Ok(Cell::from_binary(CellBinary::Number(number)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Cell, E> {
                let number = Number::deserialize(de::value::I64Deserializer::<E>::new(value))?;
                Ok(Cell::from_binary(CellBinary::Number(number)))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Cell, E> {
                let number = Number::deserialize(de::value::U64Deserializer::<E>::new(value))?;
                Ok(Cell::from_binary(CellBinary::Number(number)))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Cell, E> {
                Text::try_from(value.to_owned())
                    .map(|v| Cell::from_binary(CellBinary::Text(v)))
                    .map_err(E::custom)
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Cell, E> {
                Ok(Cell::from_binary(CellBinary::Bool(value)))
            }
        }
        if !d.is_human_readable() {
            return CellBinary::deserialize(d).map(Self::from_binary);
        }
        d.deserialize_any(ValueVisitor)
    }
}

/// Point with the same validated payloads in JSON and binary.
#[derive(Clone, PartialEq)]
pub enum Point {
    /// Value value.
    Value(Number),
    /// Pair value.
    Pair([Number; 2]),
}
impl fmt::Debug for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Point(..)")
    }
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum PointBinary {
    Value(Number),
    Pair([Number; 2]),
}
impl Point {
    fn from_binary(value: PointBinary) -> Self {
        match value {
            PointBinary::Value(v) => Self::Value(v),
            PointBinary::Pair(v) => Self::Pair(v),
        }
    }
}
impl Serialize for Point {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return match self {
                Self::Value(v) => v.serialize(s),
                Self::Pair(v) => v.serialize(s),
            };
        }
        match self {
            Self::Value(v) => PointBinary::Value(*v),
            Self::Pair(v) => PointBinary::Pair(*v),
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for Point {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Point;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a validated point")
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Point, E> {
                let number = Number::deserialize(de::value::F64Deserializer::<E>::new(value))?;
                Ok(Point::from_binary(PointBinary::Value(number)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Point, E> {
                let number = Number::deserialize(de::value::I64Deserializer::<E>::new(value))?;
                Ok(Point::from_binary(PointBinary::Value(number)))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Point, E> {
                let number = Number::deserialize(de::value::U64Deserializer::<E>::new(value))?;
                Ok(Point::from_binary(PointBinary::Value(number)))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Point, A::Error> {
                let pair = <[Number; 2]>::deserialize(de::value::SeqAccessDeserializer::new(seq))?;
                Ok(Point::from_binary(PointBinary::Pair(pair)))
            }
        }
        if !d.is_human_readable() {
            return PointBinary::deserialize(d).map(Self::from_binary);
        }
        d.deserialize_any(ValueVisitor)
    }
}

/// `KeyValue` with the same validated payloads in JSON and binary.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    /// Num value.
    Num(Finite),
    /// V2 value.
    V2([Finite; 2]),
    /// Color value.
    Color(Color),
    /// Three finite coordinates.
    V3([Finite; 3]),
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum KeyValueBinary {
    Num(Finite),
    V2([Finite; 2]),
    Color(Color),
    V3([Finite; 3]),
}
impl KeyValue {
    fn from_binary(value: KeyValueBinary) -> Self {
        match value {
            KeyValueBinary::Num(v) => Self::Num(v),
            KeyValueBinary::V2(v) => Self::V2(v),
            KeyValueBinary::Color(v) => Self::Color(v),
            KeyValueBinary::V3(v) => Self::V3(v),
        }
    }
}
impl Serialize for KeyValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return match self {
                Self::Num(v) => v.serialize(s),
                Self::V2(v) => v.serialize(s),
                Self::Color(v) => v.serialize(s),
                Self::V3(v) => v.serialize(s),
            };
        }
        match self {
            Self::Num(v) => KeyValueBinary::Num(*v),
            Self::V2(v) => KeyValueBinary::V2(*v),
            Self::Color(v) => KeyValueBinary::Color(*v),
            Self::V3(v) => KeyValueBinary::V3(*v),
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for KeyValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = KeyValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a validated keyvalue")
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<KeyValue, E> {
                let number = Finite::deserialize(de::value::F64Deserializer::<E>::new(value))?;
                Ok(KeyValue::from_binary(KeyValueBinary::Num(number)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<KeyValue, E> {
                let number = Finite::deserialize(de::value::I64Deserializer::<E>::new(value))?;
                Ok(KeyValue::from_binary(KeyValueBinary::Num(number)))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<KeyValue, E> {
                let number = Finite::deserialize(de::value::U64Deserializer::<E>::new(value))?;
                Ok(KeyValue::from_binary(KeyValueBinary::Num(number)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<KeyValue, E> {
                Color::from_text(value).map(|v| KeyValue::from_binary(KeyValueBinary::Color(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<KeyValue, A::Error> {
                let x = seq
                    .next_element::<Finite>()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let y = seq
                    .next_element::<Finite>()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let Some(z) = seq.next_element::<Finite>()? else {
                    return Ok(KeyValue::V2([x, y]));
                };
                if seq.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(4, &self));
                }
                Ok(KeyValue::V3([x, y, z]))
            }
        }
        if !d.is_human_readable() {
            return KeyValueBinary::deserialize(d).map(Self::from_binary);
        }
        d.deserialize_any(ValueVisitor)
    }
}
