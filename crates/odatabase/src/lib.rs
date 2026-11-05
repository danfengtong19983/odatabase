//! OData v4 services over SQL databases.
//!
//! Declare entities with `#[derive(ODataEntity)]`, bind them to a PostgreSQL or SQLite pool with
//! [`Service::builder`], and mount the service in an axum router with [`mount`]. Reads, writes,
//! `$batch`, `$apply`, delta links and `$metadata` then work without further code.
//!
//! ```no_run
//! use odatabase::{Nav, ODataEntity, Service};
//!
//! #[derive(ODataEntity)]
//! #[odata(table = "customer")]
//! pub struct Customer {
//!     #[odata(key)]
//!     pub id: String,
//!     pub name: String,
//!     #[odata(has_many(target = Order, on(id = "customer_id"), partner = "Customer"))]
//!     pub orders: Nav,
//! }
//!
//! #[derive(ODataEntity)]
//! #[odata(table = "orders")]
//! pub struct Order {
//!     #[odata(key, computed)]
//!     pub id: i64,
//!     pub customer_id: String,
//!     #[odata(belongs_to(target = Customer, on(customer_id = "id"), partner = "Orders"))]
//!     pub customer: Nav,
//! }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let pool = sqlx::PgPool::connect("postgres://localhost/shop").await?;
//! let service = Service::builder("Shop").entity::<Customer>().build(pool)?;
//! let app = odatabase::mount("/odata", service);
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```
//!
//! `GET /odata/Customers?$filter=startswith(Name,'A')&$expand=Orders` now answers.
//!
//! # Where to look
//!
//! - [`ODataEntity`], [`ODataComplex`] and [`ODataEnum`] list the `#[odata(...)]` attributes.
//! - [`ServiceBuilder`] registers types, operations, hooks and settings.
//! - [`Claims`] carry what the caller may see. Insert them into the request extensions from your
//!   authentication middleware: the library trusts them as given.
//! - [`ChangeHandler`], [`OperationHandler`], [`PagingProvider`], [`QueryExtension`] and
//!   [`TransientCalculator`] are the extension points.
//! - [`sql`] gives handlers access to the request's transaction.
//! - [`edm`] is the model as `$metadata` publishes it.
//!
//! # Features
//!
//! | Feature | Default | Enables |
//! |---|---|---|
//! | `postgres` | yes | PostgreSQL through `sqlx` |
//! | `sqlite` | yes | SQLite through `sqlx` |
//! | `axum` | yes | [`mount`], which serves a service from an axum router |
//!
//! Without `axum`, an adapter for another framework builds an [`ODataRequest`] and calls
//! [`Service::handle`].
//!
//! # Stability
//!
//! This crate is the supported API. The `odatabase-*` crates it depends on are published so it can
//! be, and their APIs change without notice: use them through this crate. Before 1.0, minor
//! versions may break the API, and the changelog says how.

#![warn(missing_docs)]

pub use odatabase_core::*;
pub use odatabase_edm as edm;
pub use odatabase_edm::{Binary, Nav};
pub use odatabase_macros::{ODataComplex, ODataEntity, ODataEnum};

#[cfg(feature = "axum")]
pub use odatabase_axum::mount;

/// Database access for handlers, query extensions and change tracking.
pub mod sql {
    #[cfg(feature = "postgres")]
    pub use odatabase_sql::PostgresOptions;
    pub use odatabase_sql::{
        ChangeKind, ClaimValue, Claims, ColumnDecode, ColumnValue, ColumnWrite, Database, Dialect,
        ExpandLimits, QueryError, QueryExtension, Session, TableRef, TenantScope, Tx, delete,
        insert, record_change, sea_query, update,
    };
}
