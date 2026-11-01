//! OData v4 request processing, independent of any web framework.
//!
//! An adapter turns its native request into an [`ODataRequest`] and calls [`Service::handle`].

mod format;
mod json;
mod paging;
mod read;
mod request;
mod service;
mod url;

pub use format::{Format, MetadataLevel};
pub use odatabase_sql::{
    ClaimValue, Claims, Database, Dialect, QueryError, QueryExtension, TableRef,
};
pub use paging::{OffsetPaging, PageRequest, PageResult, PagingProvider};
pub use request::{ODataError, ODataRequest, ODataResponse};
pub use service::{Config, Service, ServiceBuilder};
