#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

pub mod error;
pub mod message_types;
pub mod tables;
pub mod util;

use protobuf::{EnumFull, EnumOrUnknown, MessageField};
use serde::{Serialize, Serializer};

pub fn serialize_enum_or_unknown<E: EnumFull, S: Serializer>(
    e: &EnumOrUnknown<E>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match e.enum_value() {
        Ok(v) => s.serialize_str(v.descriptor().name()),
        Err(v) => s.serialize_i32(v),
    }
}

pub fn serialize_message_field<T, S>(
    field: &MessageField<T>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    field.as_ref().serialize(serializer)
}
