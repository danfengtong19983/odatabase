//! `#[derive(ODataEntity)]`, `#[derive(ODataComplex)]` and `#[derive(ODataEnum)]`.
//!
//! The derives emit model registration code only. Rows are described by the model at runtime,
//! so the structs themselves never hold query results.
//!
//! Part of [odatabase](https://docs.rs/odatabase). Its API follows the needs of that crate and
//! changes without notice: depend on `odatabase` instead.

#![warn(missing_docs)]

mod attrs;
mod gen_enum;
mod gen_struct;

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

/// Declares an entity type backed by a table.
///
/// The struct describes the model and is never filled with data. Fields map to columns of the
/// same name and to properties in upper camel case (`first_name` is `FirstName`). The entity set
/// is named with the plural of the type name, and the table with its snake case.
///
/// ```ignore
/// #[derive(ODataEntity)]
/// #[odata(table = "orders", set = "Orders")]
/// pub struct Order {
///     #[odata(key, computed)]
///     pub id: i64,
///     #[odata(etag)]
///     pub version: i64,
///     #[odata(max_length = 3)]
///     pub currency: Option<String>,
///     pub customer_id: String,
///     #[odata(belongs_to(target = Customer, on(customer_id = "id"), required))]
///     pub customer: Nav,
/// }
/// ```
///
/// Field types are the Rust types of Edm primitives (`bool`, integers, `f32`, `f64`,
/// `rust_decimal::Decimal`, `String`, `chrono` dates and times, `uuid::Uuid`, [`Binary`] and the
/// geospatial types), types with `ODataComplex` or `ODataEnum`, `Option<T>` for nullable
/// properties, `Vec<T>` for collections, and `Nav` for navigation properties.
///
/// # Type attributes
///
/// | Attribute | Meaning |
/// |---|---|
/// | `name = "..."` | Name of the type in the model |
/// | `table = "..."` | Table name |
/// | `set = "..."` | Name of the entity set |
/// | `no_set` | No entity set: the type is reached through navigation only |
/// | `hidden_set` | Leaves the set out of the service document |
/// | `singleton = "..."` | Exposes one entity under this name instead of a set |
/// | `abstract_type` | The type has no instances of its own |
/// | `base = Type` | Derives from another entity type |
/// | `discriminator(column = "...", value = "...")` | Single-table inheritance: rows of this type have this value |
/// | `joined(id = "id")` | Joined inheritance: this type's table is joined to the base table on these columns |
/// | `has_stream(column = "...", content_type = "...", content_type_column = "...")` | A media entity whose stream is in `column` |
/// | `track_changes` | Records changes in the change log, so reads can offer delta links |
/// | `query_extension` | Every read is filtered by a registered `QueryExtension` |
/// | `visible_for("group", ...)` | Only callers in one of the groups see the type |
/// | `annotation(term = "...", string \| bool \| int \| enum_member \| path = ...)` | Adds an annotation |
///
/// # Field attributes
///
/// | Attribute | Meaning |
/// |---|---|
/// | `key` | Part of the key |
/// | `etag` | Its value is the ETag. The service increments it on every update |
/// | `computed` | Set by the database or the server. Ignored in request bodies |
/// | `name = "..."`, `column = "..."` | Property name and column name |
/// | `max_length`, `precision`, `scale`, `srid` | Type facets |
/// | `searchable` | `$search` looks in this property |
/// | `ignore` | Not part of the model |
/// | `embedded`, `embedded(prefix = "...")` | A complex type stored in this table's columns, each named with the prefix |
/// | `collection(table = "...", owner(id = "owner_id"), value_column = "...", order_column = "...", tenant_column = "...")` | A collection stored in a child table. `value_column` is for primitive items |
/// | `transient`, `transient(requires("A", "B"))` | Calculated by a registered `TransientCalculator` from the listed properties |
/// | `description(table = "...", on(id = "id"), language_column = "...", text_column = "...")` | Language-dependent text from a description table |
/// | `protected(claim = "...")`, `protected(claim = "...", wildcards)` | Rows are visible only if this column matches the caller's claim values |
/// | `tenant`, `tenant(claim = "...")` | The tenant column. It isn't a property, and every statement is scoped by the claim, `tenant` by default |
/// | `visible_for("group", ...)` | Only callers in one of the groups see the property |
/// | `annotation(...)` | Adds an annotation, as on types |
///
/// # Navigation properties
///
/// A `Nav` field takes one of `has_many(...)`, `has_one(...)` or `belongs_to(...)`. The last two
/// mean the same: a single-valued navigation.
///
/// | Option | Meaning |
/// |---|---|
/// | `target = Type` | The related entity type |
/// | `on(source_column = "target_column", ...)` | Join columns, from this table to the target table |
/// | `join_table(table = "...", source(id = "a_id"), target(b_id = "id"), tenant_column = "...")` | Join through a link table instead of `on` |
/// | `partner = "..."` | The navigation property on the target that leads back |
/// | `required` | A single-valued navigation that is never null |
/// | `contains_target` | The targets are contained, and addressed only through this property |
///
/// [`Binary`]: https://docs.rs/odatabase/latest/odatabase/struct.Binary.html
#[proc_macro_derive(ODataEntity, attributes(odata))]
pub fn derive_entity(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    gen_struct::entity(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Declares a complex type, embedded in its owner's table or held in a collection table.
///
/// Fields take the field attributes of [`ODataEntity`] that apply to structural properties:
/// `name`, `column`, the facets, `embedded`, `computed`, `searchable` and `annotation`. The type
/// itself takes `name = "..."`.
///
/// ```ignore
/// #[derive(ODataComplex)]
/// pub struct Address {
///     pub street: Option<String>,
///     pub city: Option<String>,
/// }
/// ```
#[proc_macro_derive(ODataComplex, attributes(odata))]
pub fn derive_complex(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    gen_struct::complex(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Declares an enum type from a fieldless Rust enum.
///
/// Members take the values of the variants. The column holds the number, or the member name with
/// `as_string`.
///
/// | Attribute | Meaning |
/// |---|---|
/// | `name = "..."` | Name of the type, or on a variant, of the member |
/// | `flags` | Values combine several members as bit flags |
/// | `as_string` | The column stores member names |
///
/// ```ignore
/// #[derive(ODataEnum)]
/// #[odata(as_string)]
/// pub enum Status {
///     Open = 0,
///     Closed = 1,
/// }
/// ```
#[proc_macro_derive(ODataEnum, attributes(odata))]
pub fn derive_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    gen_enum::derive(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
