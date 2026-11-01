use crate::{Dialect, QueryError};
use odatabase_edm::{Model, Value};
use sea_query::{Alias, Expr};
use std::collections::HashMap;
use std::sync::Arc;

/// A permitted value of a claim, compared to the protected column.
#[derive(Clone, Debug, PartialEq)]
pub enum ClaimValue {
    Equals(Value),
    /// Inclusive range.
    Range(Value, Value),
}

/// What the current user may see, supplied per request by the application.
#[derive(Clone, Debug, Default)]
pub struct Claims {
    pub values: HashMap<String, Vec<ClaimValue>>,
    pub groups: Vec<String>,
}

impl Claims {
    pub fn with(mut self, claim: impl Into<String>, value: ClaimValue) -> Self {
        self.values.entry(claim.into()).or_default().push(value);
        self
    }

    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.groups.push(group.into());
        self
    }

    /// An element restricted to `groups` is visible when the user belongs to one of them.
    pub fn sees(&self, groups: &[String]) -> bool {
        groups.is_empty() || groups.iter().any(|g| self.groups.contains(g))
    }
}

/// Column access for a [`QueryExtension`], qualified with the table alias of the query.
#[derive(Clone, Debug)]
pub struct TableRef {
    pub(crate) alias: String,
}

impl TableRef {
    pub fn col(&self, column: &str) -> Expr {
        Expr::col((Alias::new(self.alias.as_str()), Alias::new(column)))
    }
}

/// Adds a condition to every read of an entity type, wherever it is reached from.
/// Registered for types declared with `#[odata(query_extension)]`.
pub trait QueryExtension: Send + Sync {
    fn condition(&self, table: &TableRef, claims: &Claims) -> Result<Option<Expr>, QueryError>;
}

/// The inputs of query translation that stay fixed during a request.
#[derive(Clone)]
pub struct QueryContext<'a> {
    pub model: &'a Model,
    pub dialect: Dialect,
    pub claims: &'a Claims,
    /// Preferred language of description properties, from `Accept-Language`.
    pub language: &'a str,
    pub extensions: &'a HashMap<String, Arc<dyn QueryExtension>>,
}
