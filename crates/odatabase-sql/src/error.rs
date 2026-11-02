/// A query could not be built or failed in the database.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The request is at fault, or is not supported. `status` is the HTTP status to return.
    #[error("{message}")]
    Status { status: u16, message: String },
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

impl QueryError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::Status {
            status: 400,
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Status {
            status: 403,
            message: message.into(),
        }
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::Status {
            status: 501,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Status {
            status: 500,
            message: message.into(),
        }
    }

    /// The HTTP status, and for request errors a message safe to show to clients.
    /// Database messages can reveal the schema, so they are never returned.
    pub fn response(&self) -> (u16, String) {
        match self {
            Self::Status { status, message } => (*status, message.clone()),
            Self::Database(e) => database_status(e),
        }
    }
}

fn database_status(e: &sqlx::Error) -> (u16, String) {
    if matches!(e, sqlx::Error::PoolTimedOut) {
        return (503, "the service is busy".into());
    }
    if let sqlx::Error::Database(db) = e {
        let code = db.code();
        let code = code.as_deref().unwrap_or("");
        let message = db.message();
        // SQLSTATE classes for Postgres, extended result codes for SQLite.
        match code {
            "23505" | "1555" | "2067" => return (409, "the entity already exists".into()),
            "23503" | "787" => return (409, "a referenced entity is missing or in use".into()),
            "23502" | "1299" => return (400, "a required property is missing".into()),
            "22012" => return (400, "division by zero".into()),
            "57014" => return (503, "the query took too long".into()),
            "22P02" | "22003" | "22007" | "22008" => {
                return (400, "a value is out of range for its type".into());
            }
            _ => {}
        }
        if message.contains("division by zero") {
            return (400, "division by zero".into());
        }
    }
    (500, "the request could not be processed".into())
}
