//! OData v4 request processing, independent of any web framework.
//!
//! An adapter turns its native request into an [`ODataRequest`] and calls [`Service::handle`].

mod batch;
mod capabilities;
mod change;
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
mod write;

pub use change::{AcceptAll, ChangeContext, ChangeHandler, EntityKey};
pub use format::{Format, MetadataLevel};
pub use odatabase_sql::{
    ClaimValue, Claims, Database, Dialect, QueryError, QueryExtension, TableRef,
};
pub use operation::{
    Binding, OperationBuilder, OperationContext, OperationHandler, OperationResult,
};
pub use paging::{OffsetPaging, PageRequest, PageResult, PagingProvider};
pub use payload::{EntityPayload, PropertyValue};
pub use request::{ODataError, ODataRequest, ODataResponse};
pub use service::{Config, Service, ServiceBuilder};
pub use transient::{TransientCalculator, TransientRow};
