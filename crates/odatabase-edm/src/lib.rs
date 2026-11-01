//! OData v4 entity data model: primitive values, the service model and CSDL writers.

mod builder;
mod csdl_json;
mod csdl_xml;
mod model;
mod value;

pub use builder::{
    Binary, EdmEntity, EdmField, FieldType, GeographyLineString, GeographyPoint, GeographyPolygon,
    GeometryLineString, GeometryPoint, GeometryPolygon, ModelBuilder, ModelError, Nav, edm_case,
    plural, snake_case,
};
pub use csdl_json::to_csdl_json;
pub use csdl_xml::to_csdl_xml;
pub use model::*;
pub use value::{PrimitiveKind, Value, ValueError, parse_duration};
