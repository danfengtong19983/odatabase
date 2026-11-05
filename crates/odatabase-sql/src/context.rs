use crate::{Dialect, QueryError};
use odatabase_edm::{Model, TenantColumn, Value};
use sea_query::{Alias, Expr};
use std::collections::HashMap;
use std::sync::Arc;

/// A permitted value of a claim, compared to the protected column.
#[derive(Clone, Debug, PartialEq)]
pub enum ClaimValue {
    /// Exactly this value.
    Equals(Value),
    /// Inclusive range.
    Range(Value, Value),
    /// Every value of a tenant claim, for an operator reading across tenants.
    Any,
}

/// Which tenants a request reaches.
#[derive(Clone, Debug, PartialEq)]
pub enum TenantScope {
    /// Every tenant, from [`ClaimValue::Any`].
    All,
    /// These tenants only.
    Only(Vec<Value>),
}

/// What the current user may see, supplied per request by the application.
#[derive(Clone, Debug, Default)]
pub struct Claims {
    /// Permitted values per claim name. A row is visible if its column matches any of them.
    pub values: HashMap<String, Vec<ClaimValue>>,
    /// The groups the user belongs to, checked against `#[odata(visible_for = "...")]`.
    pub groups: Vec<String>,
}

impl Claims {
    /// Adds a permitted value for `claim`.
    pub fn with(mut self, claim: impl Into<String>, value: ClaimValue) -> Self {
        self.values.entry(claim.into()).or_default().push(value);
        self
    }

    /// Adds a group.
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.groups.push(group.into());
        self
    }

    /// The tenants of `column` these claims reach. Without one the caller reaches no rows.
    pub fn tenant_scope(&self, column: &TenantColumn) -> Result<TenantScope, QueryError> {
        let values = self.values.get(&column.claim);
        let values = values
            .filter(|v| !v.is_empty())
            .ok_or_else(|| QueryError::forbidden(format!("claim {} is required", column.claim)))?;
        let mut tenants = Vec::with_capacity(values.len());
        for value in values {
            match value {
                ClaimValue::Any => return Ok(TenantScope::All),
                ClaimValue::Equals(v) => {
                    // Claims often arrive as text, but a uuid column compares with a uuid.
                    let typed = Value::parse_text(column.kind, &v.to_text()).map_err(|_| {
                        QueryError::forbidden(format!("claim {} is invalid", column.claim))
                    })?;
                    tenants.push(typed);
                }
                ClaimValue::Range(..) => {
                    return Err(QueryError::forbidden(format!(
                        "claim {} must name tenants",
                        column.claim
                    )));
                }
            }
        }
        Ok(TenantScope::Only(tenants))
    }

    /// The one tenant a write belongs to. A caller who reaches several must pick one first,
    /// since a write could otherwise land in, or reach into, the wrong one.
    pub fn writing_tenant(&self, column: &TenantColumn) -> Result<Value, QueryError> {
        match self.tenant_scope(column)? {
            TenantScope::Only(mut tenants) if tenants.len() == 1 => tenants
                .pop()
                .ok_or_else(|| QueryError::internal("no tenant")),
            _ => Err(QueryError::forbidden("a write needs exactly one tenant")),
        }
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
    /// A column of the table, by its database name.
    pub fn col(&self, column: &str) -> Expr {
        Expr::col((Alias::new(self.alias.as_str()), Alias::new(column)))
    }
}

/// Adds a condition to every read of an entity type, wherever it is reached from.
/// Registered for types declared with `#[odata(query_extension)]`.
pub trait QueryExtension: Send + Sync {
    /// The condition rows must meet for this caller, or `None` for no restriction.
    fn condition(&self, table: &TableRef, claims: &Claims) -> Result<Option<Expr>, QueryError>;
}

/// The inputs of query translation that stay fixed during a request.
#[doc(hidden)]
#[derive(Clone)]
pub struct QueryContext<'a> {
    pub model: &'a Model,
    pub dialect: Dialect,
    pub claims: &'a Claims,
    /// Preferred language of description properties, from `Accept-Language`.
    pub language: &'a str,
    pub extensions: &'a HashMap<String, Arc<dyn QueryExtension>>,
}
