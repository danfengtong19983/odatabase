//! Translates bound OData requests to SQL with `sea-query` and runs them with `sqlx`.
//!
//! Identifiers come from the model and literals are bound parameters, so no request text
//! reaches the SQL string.
//!
//! Part of [odatabase](https://docs.rs/odatabase). Its API follows the needs of that crate and
//! changes without notice: depend on `odatabase` instead.

#![warn(missing_docs)]

mod aggregate;
mod changes;
mod context;
mod crossjoin;
mod db;
mod dialect;
mod error;
mod expand;
mod filter;
mod query;
mod record;
mod search;
mod values;
mod write;

#[doc(hidden)]
pub use aggregate::read_aggregated;
pub use changes::{ChangeKind, record_change};
#[doc(hidden)]
pub use changes::{LoggedChange, change_floor, change_position, read_changes};
#[doc(hidden)]
pub use context::QueryContext;
pub use context::{ClaimValue, Claims, QueryExtension, TableRef, TenantScope};
#[doc(hidden)]
pub use crossjoin::{CrossJoinRow, count_crossjoin, read_crossjoin};
#[cfg(feature = "postgres")]
pub use db::PostgresOptions;
pub use db::{ColumnDecode, Database, Session, Tx};
pub use dialect::Dialect;
pub use error::QueryError;
pub use expand::ExpandLimits;
#[doc(hidden)]
pub use query::{
    Page, ReadRequest, call_scalar, count_collection, literal_args, read_collection, read_entities,
    read_sql,
};
#[doc(hidden)]
pub use record::{Node, Record, Refs};
pub use sea_query;
#[doc(hidden)]
pub use values::{from_sea_string, to_sea};
pub use write::{ColumnValue, ColumnWrite, delete, insert, update};
#[doc(hidden)]
pub use write::{collection_positions, lock_row, open_position, read_columns};
