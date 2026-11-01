//! Conversions between Edm values and database values.

use crate::{Dialect, QueryError};
use chrono::{SecondsFormat, Utc};
use odatabase_edm::{EnumType, Model, PrimitiveKind, Value};
use sea_query::Value as Sea;

/// The bound parameter for `value`. SQLite stores temporal and uuid types as text.
pub fn to_sea(model: &Model, dialect: Dialect, value: &Value) -> Result<Sea, QueryError> {
    let sqlite = dialect == Dialect::Sqlite;
    Ok(match value {
        Value::Null => Sea::String(None),
        Value::Boolean(v) => Sea::Bool(Some(*v)),
        Value::Byte(v) => Sea::SmallInt(Some(i16::from(*v))),
        Value::SByte(v) => Sea::SmallInt(Some(i16::from(*v))),
        Value::Int16(v) => Sea::SmallInt(Some(*v)),
        Value::Int32(v) => Sea::Int(Some(*v)),
        Value::Int64(v) => Sea::BigInt(Some(*v)),
        Value::Single(v) => Sea::Float(Some(*v)),
        Value::Double(v) => Sea::Double(Some(*v)),
        // SQLite has no decimal type: numeric columns hold doubles.
        Value::Decimal(v) if sqlite => Sea::Double(v.to_string().parse().ok()),
        Value::Decimal(v) => Sea::Decimal(Some(*v)),
        Value::String(v) | Value::Geo(v) => Sea::String(Some(v.clone())),
        Value::Date(_) | Value::TimeOfDay(_) if sqlite => Sea::String(Some(value.to_text())),
        Value::Date(v) => Sea::ChronoDate(Some(*v)),
        Value::TimeOfDay(v) => Sea::ChronoTime(Some(*v)),
        Value::DateTimeOffset(v) if sqlite => Sea::String(Some(
            v.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Micros, true),
        )),
        Value::DateTimeOffset(v) => Sea::ChronoDateTimeUtc(Some(v.with_timezone(&Utc))),
        Value::Duration(v) => Sea::BigInt(v.num_microseconds()),
        Value::Guid(v) if sqlite => Sea::String(Some(v.to_string())),
        Value::Guid(v) => Sea::Uuid(Some(*v)),
        Value::Binary(v) => Sea::Bytes(Some(v.clone())),
        Value::Enum { type_name, member } => {
            let e = model
                .enum_type(type_name)
                .ok_or_else(|| QueryError::internal(format!("unknown enum {type_name}")))?;
            if e.stored_as_string {
                Sea::String(Some(member.clone()))
            } else {
                Sea::BigInt(Some(enum_bits(e, member)?))
            }
        }
    })
}

/// The stored number of an enum literal: flag members are combined with `|`.
pub fn enum_bits(e: &EnumType, member: &str) -> Result<i64, QueryError> {
    member.split(',').map(str::trim).try_fold(0, |bits, m| {
        let v = e
            .member_value(m)
            .or_else(|| m.parse().ok())
            .ok_or_else(|| QueryError::bad_request(format!("{m} is not a member of {}", e.name)))?;
        Ok(bits | v)
    })
}

/// The enum value of a stored number or name.
pub fn enum_from_db(e: &EnumType, value: Value) -> Value {
    let member = match &value {
        Value::Null => return Value::Null,
        Value::String(s) => s.clone(),
        other => {
            let n = other.as_i64().unwrap_or_default();
            match e.member_name(n) {
                Some(m) => m.to_owned(),
                None if e.is_flags => e
                    .members
                    .iter()
                    .filter(|(_, v)| *v != 0 && n & v == *v)
                    .map(|(m, _)| m.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                None => n.to_string(),
            }
        }
    };
    Value::Enum {
        type_name: e.name.clone(),
        member,
    }
}

/// Reads a string column holding a typed value, as SQLite does.
pub fn from_sea_string(kind: PrimitiveKind, text: &str) -> Result<Value, QueryError> {
    match kind {
        PrimitiveKind::DateTimeOffset => {
            // SQLite date functions write `YYYY-MM-DD HH:MM:SS` without an offset.
            let normalized = match text.len() {
                19 => format!("{}Z", text.replace(' ', "T")),
                _ => text.replace(' ', "T"),
            };
            Value::parse_text(kind, &normalized)
        }
        _ => Value::parse_text(kind, text),
    }
    .map_err(|e| QueryError::internal(e.to_string()))
}
