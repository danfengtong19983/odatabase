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
| Reads | Key and navigation paths, `$filter` with lambdas and built-in functions, `$select`, `$orderby`, `$top`, `$skip`, `$count`, `$search`, `$compute`, `$schemaversion`, `$ref`, `$value`, `$entity`, `$all`, `$crossjoin`, casts, date and duration arithmetic, the same options on collection properties and inside `$select` |
| `$apply` | `aggregate`, `groupby` (with `rollup` and `$all`), `filter`, `search`, `compute`, `orderby`, `top`, `skip`, `identity`, `concat`, `join`, `outerjoin`, `expand`, and the `topcount`, `topsum`, `toppercent` family with its `bottom` forms, in pipelines, with `sum`, `min`, `max`, `average`, `countdistinct` and `$count` |
| `$expand` | Nested options, per-parent `$top` and `$skip`, `$compute` and `$apply`, `$levels`, `/$count`, `/$ref`, `*`, join tables |
| Writes | POST, PATCH, PUT, DELETE, upsert, property and `$value` updates, deep insert, deep update, `@odata.bind`, `$ref` links, ETag preconditions, `Prefer: return` |
| Delta | `Prefer: odata.track-changes`, `@odata.deltaLink`, removed entries, `Nav@delta`, `PATCH` on a collection |
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

- **Page size.** Collections return `DEFAULT_PAGE_SIZE` (1000) rows per page with a next link.
  Responses are built in memory, so `.paging(OffsetPaging { page_size: None })` makes memory use
  grow with the table.
- **Timeouts.** `Config::request_timeout` answers 503 after 60 seconds, but the database keeps
  working. Connect with `Database::connect_postgres`, or set the `statement_timeout` connect
  option on your own pool, to stop the query itself. SQLite has no equivalent.
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
- Date arithmetic is exact to the millisecond, not the microsecond.
- `matchesPattern` and table functions return 501.

### Change tracking

Mark an entity with `#[odata(track_changes)]` and create the log once at startup:

```rust
database.create_change_log().await?;
```

A read with `Prefer: odata.track-changes` then ends in an `@odata.deltaLink`. Following it returns
the entities that changed since, in their current state, and an entry with `reason` `deleted` or
`changed` for those that left the result. Each page or final response carries the next link.

- **What is logged.** Writes through the service, in their own transaction, so a failed request
  leaves no trace. An action handler or another program that writes directly must add its own row
  to `odata_changes` (`entity_set`, `entity_key`, `kind`), which a trigger can do:

  ```sql
  CREATE FUNCTION log_task() RETURNS trigger AS $$
  BEGIN
    INSERT INTO odata_changes (entity_set, entity_key, kind)
    VALUES (
      'Tasks',
      format('(%L)', CASE TG_OP WHEN 'DELETE' THEN OLD.id ELSE NEW.id END),
      CASE TG_OP WHEN 'INSERT' THEN 'C' ELSE left(TG_OP, 1) END
    );
    RETURN NULL;
  END $$ LANGUAGE plpgsql;
  CREATE TRIGGER log_task AFTER INSERT OR UPDATE OR DELETE ON task
    FOR EACH ROW EXECUTE FUNCTION log_task();
  ```

  A change seen by both the trigger and the service gets two rows, which is harmless: a client
  gets one entry.
- **Which reads get a link.** An entity set read with `$filter`, `$select`, `$orderby` or `$search`.
  `$top`, `$skip`, `$expand`, `$apply`, navigation paths, and filters on related entities or
  composable functions get a normal response: the preference is ignored, as the protocol allows.
  Links between entities aren't reported, only changes to the entities themselves.
- **Hidden rows.** A type with row protection or a query extension can't be tracked, and `build`
  says so: a removal entry would reveal the key of a row the caller can't see.
- **Retention.** Call `database.prune_change_log(age)` on a schedule. A link older than that gets
  a 410, and the client reads everything again.
- **Order on Postgres.** A position is the oldest transaction still running at the read, so a slow
  transaction can't commit behind a client's back. A long transaction can resend some rows.

### Delta payloads

`PATCH` accepts changes to part of a collection, applied as one transaction:

```json
{"Labels@delta": [
  {"Code": 3, "Body": "created or updated"},
  {"@id": "People(3)"},
  {"@removed": {"reason": "changed"}, "@id": "People(1)"},
  {"@removed": {"reason": "deleted"}, "Code": 1}
]}
```

Members the payload doesn't name stay as they are. `changed` ends the relationship, and deletes the
row only if it can't exist without its parent. `deleted` deletes the row. Removing an entity that
isn't a member does nothing, since a delta may arrive twice. The OData 4.0 form with
`$deletedEntity` works too. `PATCH` on `Set` or `Set(key)/Nav` takes the same entries in
`{"value": [...]}`, and answers 204, or the applied changes with `Prefer: return=representation`.

### Not implemented

These return a status rather than a wrong result:

| Feature | Response |
|---|---|
| Async requests | 501 |
| In `$apply`: `nest`, `ancestors`, `descendants`, `traverse`, custom aggregates, `from` with more than one level, `join` on anything but navigation | 501 |
| `join` with no later aggregation, and `expand` before an aggregation or inside `concat` | 501 |
| `$expand` after an aggregation inside `$expand` | 501 |
| `$filter` and `$orderby` on values a handler returned, as opposed to entities | 501 |
| `geo.*` and `hassubset` in `$filter` | 501 |
| Navigation properties inside complex types | 501 |
| Entity-typed operation parameters | 501 |
| Query options on the result of a handler-based operation, other than `$select` and `$expand` | 501 |
| Data formats other than JSON | 406 |
| `$link` and `$deletedLink` entries in delta payloads, `/$count` of a delta link | 400 |

`groupby` keys and aggregated values can follow to-one navigation only: a to-many path is a 400.
Each entity set announces its transformations through `Aggregation.ApplySupported`. A `concat` arm
that keeps entities leaves properties the caller can't see empty, and a filter required by
`FilterRestrictions` must hold in every arm.

A `$apply` inside `$expand` that aggregates or narrows runs one query per parent, so its cost grows
with the page size. `$expand` after an aggregation needs the target's key among the `groupby`
properties. `$search` after an aggregation looks in the string columns of the result. `$all` leaves
out sets the query doesn't fit or the caller can't read, and only fails if none is left.
`$crossjoin` has no limit of its own on the size of the product: it relies on paging and on
`statement_timeout`. `POST` to a collection property adds an item, and `$index` picks its place in
an ordered collection, with -1 for the end.

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
