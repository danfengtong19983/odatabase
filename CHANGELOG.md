# Changelog

All notable changes are listed here. The project follows [Semantic Versioning](https://semver.org):
before 1.0, a minor version may break the API, and its entry says how to migrate.

## Unreleased

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
