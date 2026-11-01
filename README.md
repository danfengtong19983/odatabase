# odatabase

An OData v4 server library for Rust. Declare entities once and get a working service over
PostgreSQL or SQLite. The design follows
[SAP/olingo-jpa-processor-v4](https://github.com/SAP/olingo-jpa-processor-v4), and also
covers what that project gets from Apache Olingo: the model, URI parser, serializers and `$batch`.

```rust
use odatabase::{Nav, ODataEntity, Service};

#[derive(ODataEntity)]
#[odata(table = "customer")]
pub struct Customer {
    #[odata(key)]
    pub id: String,
    #[odata(etag)]
    pub version: i64,
    pub name: String,
    #[odata(has_many(target = Order, on(id = "customer_id"), partner = "Customer"))]
    pub orders: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "orders")]
pub struct Order {
    #[odata(key, computed)]
    pub id: i64,
    pub customer_id: String,
    #[odata(belongs_to(target = Customer, on(customer_id = "id"), required, partner = "Orders"))]
    pub customer: Nav,
}

let service = Service::builder("Shop").entity::<Customer>().build(pool)?;
let app = odatabase::mount("/odata", service);
```

A runnable version is in [examples/quickstart.rs](crates/odatabase/examples/quickstart.rs):

```bash
cargo run -p odatabase --example quickstart
```

```bash
curl -G 'http://127.0.0.1:3000/odata/Customers' --data-urlencode "\$filter=Address/City eq 'London'" --data-urlencode '$expand=Orders'
```

The structs describe the model and are never filled: rows are read according to the model at
runtime. Field names map to columns as they are (`first_name`), and to Edm names in
upper camel case (`FirstName`). Set `name` or `column` to override either.

## What it supports

| Area | Supported |
|---|---|
| Metadata | CSDL XML and JSON, service document, `$metadata` ETag, annotations and vocabularies |
| Model | Complex types, enums and flags, collection properties, singletons, single-table and joined inheritance, media streams, geospatial properties (stored as GeoJSON) |
| Mapped properties | Language-dependent descriptions, transient properties with calculators |
| Reads | Key and navigation paths, `$filter` with lambdas and built-in functions, `$select`, `$orderby`, `$top`, `$skip`, `$count`, `$search`, `$ref`, `$value`, casts |
| `$apply` | `aggregate`, `groupby`, `filter`, `search`, `compute`, `orderby`, `top`, `skip` and `identity` in pipelines, with `sum`, `min`, `max`, `average`, `countdistinct` and `$count` |
| `$expand` | Nested options, per-parent `$top` and `$skip`, `$levels`, `/$count`, `/$ref`, `*`, join tables |
| Writes | POST, PATCH, PUT, DELETE, upsert, property and `$value` updates, deep insert, deep update, `@odata.bind`, `$ref` links, ETag preconditions, `Prefer: return` |
| `$batch` | Multipart and JSON formats, atomic change sets, `Content-ID` references, `continue-on-error` |
| Operations | Bound and unbound functions and actions, overloads, application handlers, database functions, composable table functions (Postgres) |
| Security | Row protection from claims (values, ranges, wildcards), group visibility of types, properties, navigations and operations, query extensions |
| Paging | `PagingProvider`, `@odata.nextLink`, `Prefer: odata.maxpagesize` |
| Capabilities | Enforces Insert, Update, Delete, Filter, Sort, Count and Expand restrictions, `TopSupported` and `SkipSupported` |

## Extension points

| Trait | Purpose | Register with |
|---|---|---|
| `ChangeHandler` | Validate or adjust writes inside the transaction | `.change_handler(...)` |
| `OperationHandler` | Implement functions and actions | `.function(...)`, `.action(...)` |
| `PagingProvider` | Server-driven paging | `.paging(...)` |
| `QueryExtension` | Add a condition to every read of a type | `.query_extension::<T>(...)` |
| `TransientCalculator` | Compute a transient property | `.transient::<T>(...)` |

Per-request access is set by inserting `Claims` into the request extensions from your own
middleware. Claims are only as trustworthy as that middleware: this library performs no
authentication.

## Things to know before using it

- **Set a page size.** Reads are unlimited by default, like the Java project. Public services
  should use `.paging(OffsetPaging { page_size: Some(100) })`.
- **Mounting.** `mount` returns a `Router` to merge into your own. Use it rather than `nest`,
  because `nest` answers `/odata/` with a 404 and .NET clients send that trailing slash.
- **Service URLs** are built from the `Host` and `X-Forwarded-Proto` headers. Run behind a proxy
  that sets them.
- **Bound actions** check `If-Match` before the handler runs. A handler that changes the entity
  must advance its version column too, through `sql::update`, or the old ETag stays valid.
- **Upsert.** PATCH or PUT on a missing key creates the entity and answers 201. `If-Match` stops
  that with a 412, and `If-None-Match: *` makes it insert-only. Generated keys, paths through
  navigation and types with row restrictions answer 404 instead.
- **Deep update can delete rows.** A nested array replaces the set. Rows left out are unlinked if
  the foreign key is optional, but deleted if it is required or part of their key. Join table
  rows lose only the link. Navigations the body doesn't mention are left alone. Use
  `Nav@odata.bind` to add links without replacing anything.
- **API versions** are separate `Service` values mounted at different paths.
- **Errors.** Database errors reach clients as generic messages. Causes are logged with `tracing`.

### SQLite differences

SQLite is well suited to tests and small services, with these limits:

- `Edm.Decimal` values are stored as doubles, so they lose precision beyond about 15 digits.
- `$search` is substring matching rather than text search.
- `matchesPattern` and table functions return 501.

### Not implemented

These return a status rather than a wrong result:

| Feature | Response |
|---|---|
| `$compute`, `$all`, `$crossjoin`, `$entity`, async requests | 501 |
| In `$apply`: `concat`, `nest`, `join`, `expand`, `rollup`, `topcount` and similar, custom aggregates, `from`, `Nav/$count` | 501 |
| `$expand` or `$search` next to `$apply`, and `$apply` inside `$expand` | 501 |
| Date and duration arithmetic, `maxdatetime`, `mindatetime`, `geo.*`, `hassubset` in `$filter` | 501 |
| Delta payloads (`Nav@delta`) | 501 |
| Navigation properties inside complex types | 501 |
| Entity-typed operation parameters | 501 |
| Query options on the result of a handler-based operation, other than `$select` and `$expand` | 501 |
| Data formats other than JSON | 406 |

`groupby` keys and aggregated values can follow to-one navigation only: a to-many path is a 400.
`orderby`, `top` and `skip` transformations need an aggregation later in the pipeline. `$metadata`
doesn't announce `$apply` through the `Aggregation.ApplySupported` annotation yet.

`$levels=max` stops at `ExpandLimits::max_levels`, 8 by default. `$expand` has no server-driven
paging, and `PagingProvider` applies to top-level collections only. Transient properties work
at entity level, not inside complex types.

## Differences from olingo-jpa-processor-v4

- Entities are declared with a derive rather than generated from JPA metadata, and operations
  with a builder rather than annotations on methods.
- A default SQL write path is included. The Java project requires a handler for all writes.
- To-one paths in `$filter` use correlated subqueries rather than joins.
- `$expand` selects children by the keys of the loaded parents rather than by a subquery.
- `$ref` is supported, which is on the Java project's to-do list.

## Development

```bash
cargo test --workspace
```

The test suite runs every scenario over HTTP through axum on SQLite, and again on PostgreSQL 17
in a container. Without a Docker daemon the Postgres tests print a notice and pass, except when
`CI` is set, where they fail so that a green build always means they ran.

[conformance](conformance/README.md) runs third-party OData clients against the service.

Snapshots of generated SQL and `$metadata` are in
[crates/odatabase/tests/snapshots](crates/odatabase/tests/snapshots). Review changes to them with
`cargo insta review`.

Licensed under Apache-2.0. See [NOTICE](NOTICE) for attribution.
