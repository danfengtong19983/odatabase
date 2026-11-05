# Contributing

Thanks for helping. Bug reports, failing requests from a real OData client, and pull requests
are all welcome.

## Reporting a bug

Open an issue with the request (method, URL, body), the response you got, and the response you
expected. If a client library sent the request, name it and its version. A failing test is the
most useful report of all.

Report security problems privately instead: see [SECURITY.md](SECURITY.md).

## Building and testing

```bash
cargo test --workspace
```

The suite runs every scenario over HTTP through axum on SQLite, and again on PostgreSQL 17 in a
container through `testcontainers`. Without a Docker daemon the Postgres tests print a notice and
pass, so start Docker before you trust a green run. On CI they fail instead.

Before opening a pull request, run what CI runs:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

- **Snapshots.** Generated SQL and `$metadata` are pinned in
  `crates/odatabase/tests/snapshots`. Review changes with `cargo insta review` and commit them
  with the change that caused them.
- **Clients.** `conformance/` runs third-party OData clients (.NET, Java, Python, UI5 and an
  OpenAPI converter) against a test server. CI runs them on every pull request. See
  [conformance/README.md](conformance/README.md) to run one locally.

## Making changes

- **Tests.** A new feature or fix comes with a test in `crates/odatabase/tests` that sends the
  request over HTTP. Run it on both databases.
- **Unsupported is better than wrong.** If a combination of features isn't handled, answer 501
  or 400 with a clear message, and list it under "Not implemented" in the README.
- **Safety.** Identifiers in SQL come from the model, and values are always bound parameters.
  Row protection, tenancy, query extensions and visibility must hold wherever a type is
  reached from: paths, `$expand`, `$filter`, lambdas, `$apply`, `$batch` and counts.
- **Public API.** Everything reachable from the `odatabase` crate needs documentation, which
  `missing_docs` enforces. The `odatabase-*` crates are internal: change them freely, but mark
  items that must be public only for other crates with `#[doc(hidden)]`.
- **Changelog.** Add a line to the Unreleased section of [CHANGELOG.md](CHANGELOG.md) for any
  change users will notice, and say how to migrate if it breaks something.

## License

By contributing, you agree that your contributions are licensed under the Apache License 2.0,
like the rest of the project.
