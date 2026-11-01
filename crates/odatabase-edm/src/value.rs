//! Edm primitive kinds and the dynamic [`Value`] used for literals, rows and payloads.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, SecondsFormat, TimeDelta};
use rust_decimal::Decimal;
use std::str::FromStr;
use uuid::Uuid;

/// The primitive types of the entity data model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrimitiveKind {
    Boolean,
    Byte,
    SByte,
    Int16,
    Int32,
    Int64,
    Single,
    Double,
    Decimal,
    String,
    Date,
    DateTimeOffset,
    TimeOfDay,
    Duration,
    Guid,
    Binary,
    GeographyPoint,
    GeographyLineString,
    GeographyPolygon,
    GeometryPoint,
    GeometryLineString,
    GeometryPolygon,
}

impl PrimitiveKind {
    pub fn edm_name(self) -> &'static str {
        match self {
            Self::Boolean => "Edm.Boolean",
            Self::Byte => "Edm.Byte",
            Self::SByte => "Edm.SByte",
            Self::Int16 => "Edm.Int16",
            Self::Int32 => "Edm.Int32",
            Self::Int64 => "Edm.Int64",
            Self::Single => "Edm.Single",
            Self::Double => "Edm.Double",
            Self::Decimal => "Edm.Decimal",
            Self::String => "Edm.String",
            Self::Date => "Edm.Date",
            Self::DateTimeOffset => "Edm.DateTimeOffset",
            Self::TimeOfDay => "Edm.TimeOfDay",
            Self::Duration => "Edm.Duration",
            Self::Guid => "Edm.Guid",
            Self::Binary => "Edm.Binary",
            Self::GeographyPoint => "Edm.GeographyPoint",
            Self::GeographyLineString => "Edm.GeographyLineString",
            Self::GeographyPolygon => "Edm.GeographyPolygon",
            Self::GeometryPoint => "Edm.GeometryPoint",
            Self::GeometryLineString => "Edm.GeometryLineString",
            Self::GeometryPolygon => "Edm.GeometryPolygon",
        }
    }

    pub fn from_edm_name(name: &str) -> Option<Self> {
        const ALL: [PrimitiveKind; 22] = [
            PrimitiveKind::Boolean,
            PrimitiveKind::Byte,
            PrimitiveKind::SByte,
            PrimitiveKind::Int16,
            PrimitiveKind::Int32,
            PrimitiveKind::Int64,
            PrimitiveKind::Single,
            PrimitiveKind::Double,
            PrimitiveKind::Decimal,
            PrimitiveKind::String,
            PrimitiveKind::Date,
            PrimitiveKind::DateTimeOffset,
            PrimitiveKind::TimeOfDay,
            PrimitiveKind::Duration,
            PrimitiveKind::Guid,
            PrimitiveKind::Binary,
            PrimitiveKind::GeographyPoint,
            PrimitiveKind::GeographyLineString,
            PrimitiveKind::GeographyPolygon,
            PrimitiveKind::GeometryPoint,
            PrimitiveKind::GeometryLineString,
            PrimitiveKind::GeometryPolygon,
        ];
        ALL.into_iter().find(|k| k.edm_name() == name)
    }

    pub fn is_numeric(self) -> bool {
        self.is_integer() || matches!(self, Self::Single | Self::Double | Self::Decimal)
    }

    pub fn is_integer(self) -> bool {
        matches!(
            self,
            Self::Byte | Self::SByte | Self::Int16 | Self::Int32 | Self::Int64
        )
    }

    pub fn is_geo(self) -> bool {
        matches!(
            self,
            Self::GeographyPoint
                | Self::GeographyLineString
                | Self::GeographyPolygon
                | Self::GeometryPoint
                | Self::GeometryLineString
                | Self::GeometryPolygon
        )
    }

    /// True when the OData JSON format represents the kind as a JSON number by default.
    /// `Edm.Int64` and `Edm.Decimal` are numbers unless `IEEE754Compatible=true`.
    pub fn is_ieee754_sensitive(self) -> bool {
        matches!(self, Self::Int64 | Self::Decimal)
    }
}

/// A dynamically typed Edm value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Byte(u8),
    SByte(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Single(f32),
    Double(f64),
    Decimal(Decimal),
    String(String),
    Date(NaiveDate),
    DateTimeOffset(DateTime<FixedOffset>),
    TimeOfDay(NaiveTime),
    Duration(TimeDelta),
    Guid(Uuid),
    Binary(Vec<u8>),
    /// Member of an enum type, addressed by qualified type name and member name.
    Enum {
        type_name: String,
        member: String,
    },
    /// GeoJSON document of a geospatial property.
    Geo(String),
}

/// A literal could not be read as the requested kind.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("invalid {kind} value: {text}")]
pub struct ValueError {
    pub kind: &'static str,
    pub text: String,
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn kind(&self) -> Option<PrimitiveKind> {
        Some(match self {
            Self::Null | Self::Enum { .. } | Self::Geo(_) => return None,
            Self::Boolean(_) => PrimitiveKind::Boolean,
            Self::Byte(_) => PrimitiveKind::Byte,
            Self::SByte(_) => PrimitiveKind::SByte,
            Self::Int16(_) => PrimitiveKind::Int16,
            Self::Int32(_) => PrimitiveKind::Int32,
            Self::Int64(_) => PrimitiveKind::Int64,
            Self::Single(_) => PrimitiveKind::Single,
            Self::Double(_) => PrimitiveKind::Double,
            Self::Decimal(_) => PrimitiveKind::Decimal,
            Self::String(_) => PrimitiveKind::String,
            Self::Date(_) => PrimitiveKind::Date,
            Self::DateTimeOffset(_) => PrimitiveKind::DateTimeOffset,
            Self::TimeOfDay(_) => PrimitiveKind::TimeOfDay,
            Self::Duration(_) => PrimitiveKind::Duration,
            Self::Guid(_) => PrimitiveKind::Guid,
            Self::Binary(_) => PrimitiveKind::Binary,
        })
    }

    /// Parses the plain text form of a value, as used in JSON strings and `$value` bodies.
    pub fn parse_text(kind: PrimitiveKind, text: &str) -> Result<Self, ValueError> {
        let err = || ValueError {
            kind: kind.edm_name(),
            text: text.to_owned(),
        };
        Ok(match kind {
            PrimitiveKind::Boolean => Self::Boolean(text.parse().map_err(|_| err())?),
            PrimitiveKind::Byte => Self::Byte(text.parse().map_err(|_| err())?),
            PrimitiveKind::SByte => Self::SByte(text.parse().map_err(|_| err())?),
            PrimitiveKind::Int16 => Self::Int16(text.parse().map_err(|_| err())?),
            PrimitiveKind::Int32 => Self::Int32(text.parse().map_err(|_| err())?),
            PrimitiveKind::Int64 => Self::Int64(text.parse().map_err(|_| err())?),
            PrimitiveKind::Single => Self::Single(parse_float(text).ok_or_else(err)? as f32),
            PrimitiveKind::Double => Self::Double(parse_float(text).ok_or_else(err)?),
            PrimitiveKind::Decimal => Self::Decimal(
                Decimal::from_str(text)
                    .or_else(|_| Decimal::from_scientific(text))
                    .map_err(|_| err())?,
            ),
            PrimitiveKind::String => Self::String(text.to_owned()),
            PrimitiveKind::Date => {
                Self::Date(NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| err())?)
            }
            PrimitiveKind::DateTimeOffset => {
                Self::DateTimeOffset(DateTime::parse_from_rfc3339(text).map_err(|_| err())?)
            }
            PrimitiveKind::TimeOfDay => Self::TimeOfDay(
                NaiveTime::parse_from_str(text, "%H:%M:%S%.f")
                    .or_else(|_| NaiveTime::parse_from_str(text, "%H:%M"))
                    .map_err(|_| err())?,
            ),
            PrimitiveKind::Duration => Self::Duration(parse_duration(text).ok_or_else(err)?),
            PrimitiveKind::Guid => Self::Guid(Uuid::parse_str(text).map_err(|_| err())?),
            PrimitiveKind::Binary => Self::Binary(
                URL_SAFE_NO_PAD
                    .decode(text.trim_end_matches('='))
                    .map_err(|_| err())?,
            ),
            k if k.is_geo() => Self::Geo(text.to_owned()),
            _ => return Err(err()),
        })
    }

    /// Plain text form, the inverse of [`Value::parse_text`]. `Null` renders as an empty string.
    pub fn to_text(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Boolean(v) => v.to_string(),
            Self::Byte(v) => v.to_string(),
            Self::SByte(v) => v.to_string(),
            Self::Int16(v) => v.to_string(),
            Self::Int32(v) => v.to_string(),
            Self::Int64(v) => v.to_string(),
            Self::Single(v) => format_float(*v as f64),
            Self::Double(v) => format_float(*v),
            Self::Decimal(v) => v.normalize().to_string(),
            Self::String(v) | Self::Geo(v) => v.clone(),
            Self::Date(v) => v.format("%Y-%m-%d").to_string(),
            Self::DateTimeOffset(v) => v.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            Self::TimeOfDay(v) => v.format("%H:%M:%S%.f").to_string(),
            Self::Duration(v) => format_duration(*v),
            Self::Guid(v) => v.to_string(),
            Self::Binary(v) => URL_SAFE_NO_PAD.encode(v),
            Self::Enum { member, .. } => member.clone(),
        }
    }

    /// The form used inside a URL, such as a key predicate: strings are quoted.
    pub fn to_uri_literal(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::String(v) => format!("'{}'", v.replace('\'', "''")),
            Self::Binary(_) => format!("binary'{}'", self.to_text()),
            Self::Duration(_) => format!("duration'{}'", self.to_text()),
            Self::Enum { type_name, member } => format!("{type_name}'{member}'"),
            other => other.to_text(),
        }
    }

    /// Converts a numeric or string value to `kind`, returning `None` when that loses the value.
    pub fn coerce(&self, kind: PrimitiveKind) -> Option<Self> {
        if self.is_null() || self.kind() == Some(kind) {
            return Some(self.clone());
        }
        if let Some(i) = self.as_i64() {
            return Some(match kind {
                PrimitiveKind::Byte => Self::Byte(u8::try_from(i).ok()?),
                PrimitiveKind::SByte => Self::SByte(i8::try_from(i).ok()?),
                PrimitiveKind::Int16 => Self::Int16(i16::try_from(i).ok()?),
                PrimitiveKind::Int32 => Self::Int32(i32::try_from(i).ok()?),
                PrimitiveKind::Int64 => Self::Int64(i),
                PrimitiveKind::Single => Self::Single(i as f32),
                PrimitiveKind::Double => Self::Double(i as f64),
                PrimitiveKind::Decimal => Self::Decimal(Decimal::from(i)),
                _ => return None,
            });
        }
        match (self, kind) {
            (Self::Decimal(d), PrimitiveKind::Double) => {
                Some(Self::Double(d.to_string().parse().ok()?))
            }
            (Self::Decimal(d), PrimitiveKind::Single) => {
                Some(Self::Single(d.to_string().parse().ok()?))
            }
            (Self::Double(d), PrimitiveKind::Single) => Some(Self::Single(*d as f32)),
            (Self::Single(d), PrimitiveKind::Double) => Some(Self::Double(*d as f64)),
            (Self::Double(d), PrimitiveKind::Decimal) => {
                Some(Self::Decimal(Decimal::from_str(&d.to_string()).ok()?))
            }
            (Self::Single(d), PrimitiveKind::Decimal) => {
                Some(Self::Decimal(Decimal::from_str(&d.to_string()).ok()?))
            }
            (Self::String(s), k) if k.is_geo() => Some(Self::Geo(s.clone())),
            (Self::Date(d), PrimitiveKind::DateTimeOffset) => Some(Self::DateTimeOffset(
                d.and_hms_opt(0, 0, 0)?.and_utc().fixed_offset(),
            )),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Byte(v) => Some(i64::from(*v)),
            Self::SByte(v) => Some(i64::from(*v)),
            Self::Int16(v) => Some(i64::from(*v)),
            Self::Int32(v) => Some(i64::from(*v)),
            Self::Int64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(v) => Some(v),
            _ => None,
        }
    }
}

fn parse_float(text: &str) -> Option<f64> {
    match text {
        "INF" => Some(f64::INFINITY),
        "-INF" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => text.parse().ok(),
    }
}

fn format_float(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 {
            "INF".to_owned()
        } else {
            "-INF".to_owned()
        }
    } else {
        v.to_string()
    }
}

/// Reads an xsd:dayTimeDuration such as `-P1DT2H3M4.5S`.
pub fn parse_duration(text: &str) -> Option<TimeDelta> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let rest = rest.strip_prefix('P')?;
    if rest.is_empty() {
        return None;
    }
    let (date, time) = match rest.split_once('T') {
        Some((_, "")) => return None,
        Some((d, t)) => (d, t),
        None => (rest, ""),
    };
    let mut micros: i128 = 0;
    if !date.is_empty() {
        let days: i128 = date.strip_suffix('D')?.parse().ok()?;
        micros += days * 86_400_000_000;
    }
    let mut number = String::new();
    for c in time.chars() {
        match c {
            '0'..='9' | '.' => number.push(c),
            'H' | 'M' | 'S' => {
                let value: f64 = number.parse().ok()?;
                number.clear();
                let unit = match c {
                    'H' => 3_600_000_000.0,
                    'M' => 60_000_000.0,
                    _ => 1_000_000.0,
                };
                micros += (value * unit).round() as i128;
            }
            _ => return None,
        }
    }
    if !number.is_empty() {
        return None;
    }
    let micros = i64::try_from(if negative { -micros } else { micros }).ok()?;
    Some(TimeDelta::microseconds(micros))
}

fn format_duration(d: TimeDelta) -> String {
    let total = d.num_microseconds().unwrap_or(i64::MAX);
    let sign = if total < 0 { "-" } else { "" };
    let abs = total.unsigned_abs();
    let days = abs / 86_400_000_000;
    let rem = abs % 86_400_000_000;
    let (hours, rem) = (rem / 3_600_000_000, rem % 3_600_000_000);
    let (mins, rem) = (rem / 60_000_000, rem % 60_000_000);
    let (secs, micros) = (rem / 1_000_000, rem % 1_000_000);
    let mut out = format!("{sign}P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if hours > 0 || mins > 0 || secs > 0 || micros > 0 || days == 0 {
        out.push('T');
        if hours > 0 {
            out.push_str(&format!("{hours}H"));
        }
        if mins > 0 {
            out.push_str(&format!("{mins}M"));
        }
        if micros > 0 {
            let frac = format!("{micros:06}");
            out.push_str(&format!("{secs}.{}S", frac.trim_end_matches('0')));
        } else if secs > 0 || (hours == 0 && mins == 0) {
            out.push_str(&format!("{secs}S"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let cases = [
            (PrimitiveKind::Boolean, "true"),
            (PrimitiveKind::Int32, "-7"),
            (PrimitiveKind::Decimal, "12.5"),
            (PrimitiveKind::Date, "2026-10-31"),
            (PrimitiveKind::DateTimeOffset, "2026-10-31T12:30:00Z"),
            (PrimitiveKind::TimeOfDay, "12:30:15"),
            (PrimitiveKind::Duration, "P1DT2H3M4.5S"),
            (PrimitiveKind::Duration, "-PT5M"),
            (PrimitiveKind::Guid, "01234567-89ab-cdef-0123-456789abcdef"),
            (PrimitiveKind::Binary, "AQID"),
            (PrimitiveKind::Double, "INF"),
        ];
        for (kind, text) in cases {
            assert_eq!(Value::parse_text(kind, text).unwrap().to_text(), text);
        }
    }

    #[test]
    fn invalid_text_is_rejected() {
        assert!(Value::parse_text(PrimitiveKind::Byte, "300").is_err());
        assert!(Value::parse_text(PrimitiveKind::Duration, "P1Y").is_err());
        assert!(Value::parse_text(PrimitiveKind::Date, "2026-13-01").is_err());
    }

    #[test]
    fn coercion_refuses_overflow() {
        assert_eq!(
            Value::Int64(5).coerce(PrimitiveKind::Byte),
            Some(Value::Byte(5))
        );
        assert_eq!(Value::Int64(500).coerce(PrimitiveKind::Byte), None);
        assert_eq!(Value::String("x".into()).coerce(PrimitiveKind::Int32), None);
    }

    #[test]
    fn uri_literal_escapes_quotes() {
        assert_eq!(Value::String("O'Neil".into()).to_uri_literal(), "'O''Neil'");
    }
}
