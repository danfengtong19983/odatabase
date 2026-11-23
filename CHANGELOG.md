# Changelog

All notable changes are listed here. The project follows [Semantic Versioning](https://semver.org):
before 1.0, a minor version may break the API, and its entry says how to migrate.

## Unreleased

- **Open types.** `#[odata(open_type(column = "..."))]` keeps an entity type's dynamic
  properties as the members of a JSON object in one column (`jsonb`, `json` or text on Postgres,
  text on SQLite). They are read, selected, filtered, sorted and written like declared
  properties, including members of nested objects in `$filter`. See "Open types" in the README.
- **Breaking:** `EntityType` has a new `open` field, `EntityPayload` a new `dynamic` field and
  `sql::ColumnWrite` a new `json` field, with `ColumnWrite::json` and `sql::JsonWrite` to fill
  it. Code that builds these with struct literals needs the new field.

- **JSON properties.** A `serde_json::Value` field is an `Edm.Untyped` property: any JSON
  value, stored whole in a `jsonb`, `json` or text column on Postgres and text on SQLite. It can
  be selected and checked against null, not compared or sorted. See "JSON properties" in the
  README. Postgres reads `json` and `jsonb` columns as text wherever they appear.
- **Breaking:** `PrimitiveKind` has a new `Untyped` variant and `Value` a new `Json` variant.
  Exhaustive matches on them need an arm.

- **`rename_all`.** `#[odata(rename_all = "snake_case")]`, `"camelCase"` or `"PascalCase"` on an
  entity or complex type names the properties and navigation properties of its fields without a
  `name`. The default stays `PascalCase`.
- **Fix:** a field with a raw identifier such as `r#type` now maps to the column `type` and the
  property `Type`, rather than `r#type` and `R#type`.

## 0.1.0 - 2026-11-16

First release.

- **Model.** `#[derive(ODataEntity)]`, `ODataComplex` and `ODataEnum` over PostgreSQL and SQLite,
  with complex types, enums and flags, collection properties, singletons, single-table and joined
  inheritance, media streams, geospatial properties, language-dependent descriptions, transient
  properties, annotations, and `$metadata` as CSDL XML and JSON.
- **Reads.** Key and navigation paths, `$filter` with lambdas and built-in functions, `$select`,
  `$orderby`, `$top`, `$skip`, `$count`, `$search`, `$compute`, `$expand` with nested options and
  `$levels`, `$ref`, `$value`, `$entity`, `$all`, `$crossjoin`, `$schemaversion`, and `$apply`.
- **Writes.** POST, PATCH, PUT and DELETE, upsert, deep insert and update, `@odata.bind`, `$ref`
  links, ETag preconditions and `Prefer: return`.
- **Delta.** `Prefer: odata.track-changes` with a change log table, delta links, and `Nav@delta`
  payloads.
- **`$batch`** in multipart and JSON formats, with atomic change sets.
- **Operations.** Bound and unbound functions and actions, application handlers and database
  functions.
- **Security.** Row protection from claims, group visibility, query extensions, and
  multi-tenancy with `#[odata(tenant)]` and settings for PostgreSQL row-level security.
- **Integration.** `mount` for axum, and `Service::handle` for other frameworks.
- **Limits.** `Config::public_root` for links behind a proxy, `max_body_size` and
  `max_batch_requests`, and `UriLimits` bounds on `in` lists, `$orderby`, `$select` and
  `matchesPattern`.
