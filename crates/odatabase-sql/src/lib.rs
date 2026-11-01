//! Translates bound OData requests to SQL with `sea-query` and runs them with `sqlx`.
//!
//! Identifiers come from the model and literals are bound parameters, so no request text
//! reaches the SQL string.

mod aggregate;
mod context;
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

pub use aggregate::read_aggregated;
pub use context::{ClaimValue, Claims, QueryContext, QueryExtension, TableRef};
pub use db::{ColumnDecode, Database, Session, Tx};
pub use dialect::Dialect;
pub use error::QueryError;
pub use expand::ExpandLimits;
pub use query::{
    Page, ReadRequest, call_scalar, count_collection, literal_args, read_collection, read_entities,
    read_sql,
};
pub use record::{Node, Record, Refs};
pub use sea_query;
pub use values::{from_sea_string, to_sea};
pub use write::{ColumnValue, ColumnWrite, delete, insert, lock_row, read_columns, update};
