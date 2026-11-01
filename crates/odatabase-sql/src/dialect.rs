use odatabase_edm::PrimitiveKind;

/// The supported SQL dialects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    /// `$1 IS NOT DISTINCT FROM $2`: equality where nulls compare equal.
    pub fn null_safe_eq(self) -> &'static str {
        match self {
            Self::Postgres => "($1 IS NOT DISTINCT FROM $2)",
            Self::Sqlite => "($1 IS $2)",
        }
    }

    pub fn null_safe_ne(self) -> &'static str {
        match self {
            Self::Postgres => "($1 IS DISTINCT FROM $2)",
            Self::Sqlite => "($1 IS NOT $2)",
        }
    }

    pub fn cast_type(self, kind: PrimitiveKind) -> Option<&'static str> {
        use PrimitiveKind as P;
        Some(match (self, kind) {
            (Self::Postgres, P::Boolean) => "boolean",
            (Self::Postgres, P::Byte | P::SByte | P::Int16) => "smallint",
            (Self::Postgres, P::Int32) => "integer",
            (Self::Postgres, P::Int64 | P::Duration) => "bigint",
            (Self::Postgres, P::Single) => "real",
            (Self::Postgres, P::Double) => "double precision",
            (Self::Postgres, P::Decimal) => "numeric",
            (Self::Postgres, P::String) => "text",
            (Self::Postgres, P::Date) => "date",
            (Self::Postgres, P::DateTimeOffset) => "timestamptz",
            (Self::Postgres, P::TimeOfDay) => "time",
            (Self::Postgres, P::Guid) => "uuid",
            (Self::Postgres, P::Binary) => "bytea",
            (Self::Sqlite, P::Boolean | P::Byte | P::SByte | P::Int16 | P::Int32 | P::Int64) => {
                "INTEGER"
            }
            (Self::Sqlite, P::Duration) => "INTEGER",
            (Self::Sqlite, P::Single | P::Double | P::Decimal) => "REAL",
            (Self::Sqlite, P::Binary) => "BLOB",
            (Self::Sqlite, P::String | P::Date | P::DateTimeOffset | P::TimeOfDay | P::Guid) => {
                "TEXT"
            }
            _ => return None,
        })
    }
}
