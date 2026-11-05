//! OData v4 request processing, independent of any web framework.
//!
//! An adapter turns its native request into an [`ODataRequest`] and calls [`Service::handle`].
//!
//! Part of [odatabase](https://docs.rs/odatabase). Its API follows the needs of that crate and
//! changes without notice: depend on `odatabase` instead.

#![warn(missing_docs)]

mod batch;
mod capabilities;
mod change;
mod delta;
mod format;
mod json;
mod operation;
mod paging;
mod payload;
mod read;
mod request;
mod service;
mod transient;
mod url;
mod visibility;
mod wide;
mod write;

pub use change::{AcceptAll, ChangeContext, ChangeHandler, EntityKey};
pub use format::{Format, MetadataLevel};
#[cfg(feature = "postgres")]
pub use odatabase_sql::PostgresOptions;
pub use odatabase_sql::TenantScope;
pub use odatabase_sql::{
    ClaimValue, Claims, Database, Dialect, ExpandLimits, QueryError, QueryExtension, TableRef,
};
pub use odatabase_uri::Limits as UriLimits;
pub use operation::{
    Binding, OperationBuilder, OperationContext, OperationHandler, OperationResult,
};
pub use paging::{DEFAULT_PAGE_SIZE, OffsetPaging, PageRequest, PageResult, PagingProvider};
pub use payload::{DeltaItem, EntityPayload, PropertyValue};
pub use request::{ODataError, ODataRequest, ODataResponse};
pub use service::{Config, Service, ServiceBuilder};
pub use transient::{TransientCalculator, TransientRow};
