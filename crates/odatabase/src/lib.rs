//! OData v4 services over SQL databases.
//!
//! Declare entities with `#[derive(ODataEntity)]`, bind them to a database with
//! [`Service::builder`], and mount the service with [`mount`].

pub use odatabase_core::*;
pub use odatabase_edm as edm;
pub use odatabase_edm::{Binary, Nav};
pub use odatabase_macros::{ODataComplex, ODataEntity, ODataEnum};
pub use odatabase_sql as sql;
pub use odatabase_uri as uri;

#[cfg(feature = "axum")]
pub use odatabase_axum::mount;
