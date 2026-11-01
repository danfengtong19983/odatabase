//! Hooks around the default SQL writes, the counterpart of `JPACUDRequestHandler`.

use crate::ODataError;
use crate::payload::EntityPayload;
use async_trait::async_trait;
use http::Method;
use odatabase_edm::{Model, Value};
use odatabase_sql::{Claims, Session};

/// What a hook can see of the running change.
pub struct ChangeContext<'a> {
    pub model: &'a Model,
    pub claims: &'a Claims,
    /// The transaction of the change. Statements run here commit or roll back with it.
    pub session: &'a Session,
    pub method: &'a Method,
}

/// The key of the entity a change applies to.
pub type EntityKey = Vec<(String, Value)>;

/// Validates or adjusts changes. Every method defaults to accepting the change.
/// A returned error rolls the transaction back and becomes the response.
#[async_trait]
pub trait ChangeHandler: Send + Sync {
    async fn before_create(
        &self,
        _ctx: &ChangeContext<'_>,
        _entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Ok(())
    }

    async fn before_update(
        &self,
        _ctx: &ChangeContext<'_>,
        _key: &EntityKey,
        _entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Ok(())
    }

    async fn before_delete(
        &self,
        _ctx: &ChangeContext<'_>,
        _type_name: &str,
        _key: &EntityKey,
    ) -> Result<(), ODataError> {
        Ok(())
    }

    /// Runs after all changes of a request or change set, before the commit.
    async fn validate_changes(&self, _ctx: &ChangeContext<'_>) -> Result<(), ODataError> {
        Ok(())
    }
}

/// The handler used when the application registers none.
pub struct AcceptAll;

#[async_trait]
impl ChangeHandler for AcceptAll {}
