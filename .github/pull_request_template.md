## What and why

<!-- What the change does, and the problem it solves. Link the issue if there is one. -->

## Checklist

- [ ] A test in `crates/odatabase/tests` sends the request over HTTP, and passes on SQLite and Postgres
- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] Snapshot changes in `crates/odatabase/tests/snapshots` are reviewed and intended
- [ ] A line in the Unreleased section of CHANGELOG.md, if users will notice the change
- [ ] Unsupported combinations answer 501 or 400 and are listed under "Not implemented" in the README
