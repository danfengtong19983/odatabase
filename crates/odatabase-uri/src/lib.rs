//! Parses OData v4 URLs into a typed AST, resolving every name against the service model.
//!
//! Parsing and binding are one pass: an [`ODataUri`] that exists refers only to elements of
//! the model, so later stages never see user-chosen identifiers.

mod apply;
mod ast;
mod error;
mod expr;
mod lexer;
mod options;
mod path;

pub use apply::SUPPORTED as APPLY_TRANSFORMATIONS;
pub use ast::*;
pub use error::UriError;
pub use path::{Limits, parse, parse_with, take_entity_id};
