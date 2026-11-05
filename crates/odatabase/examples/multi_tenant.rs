//! A multi-tenant task service with claims from middleware, a change handler and an action.
//!
//! ```bash
//! cargo run -p odatabase --example multi_tenant
//! curl -H 'X-Tenant: acme' http://127.0.0.1:3000/odata/Tasks
//! curl -H 'X-Tenant: acme' -H 'Content-Type: application/json' \
//!      -d '{"Title": "  Ship it  "}' http://127.0.0.1:3000/odata/Tasks
//! curl -H 'X-Tenant: acme' -X POST http://127.0.0.1:3000/odata/Tasks(1)/Shop.Complete
//! ```
//!
//! The `X-Tenant` header stands in for real authentication: take the claims from a verified
//! token instead, because the service trusts them as given.

use async_trait::async_trait;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use odatabase::edm::Value;
use odatabase::sql::{ColumnWrite, update};
use odatabase::{
    Binding, ChangeContext, ChangeHandler, ClaimValue, Claims, EntityKey, EntityPayload,
    ODataEntity, ODataError, OperationContext, OperationHandler, OperationResult, PropertyValue,
    Service,
};

#[derive(ODataEntity)]
#[odata(table = "task")]
pub struct Task {
    #[odata(tenant)]
    pub tenant_id: String,
    #[odata(key, computed)]
    pub id: i64,
    #[odata(max_length = 200)]
    pub title: String,
    #[odata(computed)]
    pub done: bool,
}

const SCHEMA: &str = "
CREATE TABLE task (
  tenant_id TEXT NOT NULL, id INTEGER PRIMARY KEY, title TEXT NOT NULL,
  done BOOLEAN NOT NULL DEFAULT FALSE
);
INSERT INTO task (tenant_id, title) VALUES ('acme', 'Write docs'), ('globex', 'Hire');
";

/// Turns the `X-Tenant` header into the `tenant` claim. A request without it gets a 401.
async fn claims(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    let tenant = request
        .headers()
        .get("x-tenant")
        .and_then(|h| h.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_owned();
    let claims = Claims::default().with("tenant", ClaimValue::Equals(Value::String(tenant)));
    request.extensions_mut().insert(claims);
    Ok(next.run(request).await)
}

/// Trims titles and refuses empty ones, on create and update alike.
struct Validate;

impl Validate {
    fn title(entity: &mut EntityPayload) -> Result<(), ODataError> {
        for (name, value) in &mut entity.values {
            if let ("Title", PropertyValue::Value(Value::String(s))) = (name.as_str(), value) {
                *s = s.trim().to_owned();
                if s.is_empty() {
                    return Err(ODataError::bad_request("Title must not be empty"));
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ChangeHandler for Validate {
    async fn before_create(
        &self,
        _: &ChangeContext<'_>,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Self::title(entity)
    }

    async fn before_update(
        &self,
        _: &ChangeContext<'_>,
        _: &EntityKey,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Self::title(entity)
    }
}

/// `POST Tasks(1)/Shop.Complete`. The service has already checked that the caller can see the
/// task, and the statement runs in the request's transaction.
struct Complete;

#[async_trait]
impl OperationHandler for Complete {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let Some(Binding::Entity { key, .. }) = &ctx.binding else {
            return Err(ODataError::bad_request("Complete is bound to a task"));
        };
        // Statements a handler writes itself aren't scoped for it: name the tenant too.
        let Some([ClaimValue::Equals(tenant)]) = ctx.claims.values.get("tenant").map(Vec::as_slice)
        else {
            return Err(ODataError::new(403, "one tenant required"));
        };
        update(
            ctx.model,
            ctx.session,
            "task",
            &[ColumnWrite::new("done", Value::Boolean(true), None)],
            None,
            &[
                ("id".to_owned(), key[0].1.clone()),
                ("tenant_id".to_owned(), tenant.clone()),
            ],
        )
        .await?;
        Ok(OperationResult::Void)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;

    let service = Service::builder("Shop")
        .entity::<Task>()
        .change_handler(Validate)
        .action("Complete", |a| a.bound_to::<Task>().handler(Complete))
        .build(pool)?;
    let app = odatabase::mount("/odata", service).layer(axum::middleware::from_fn(claims));

    let address = std::env::var("ODATA_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_owned());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("serving http://{address}/odata");
    axum::serve(listener, app).await?;
    Ok(())
}
