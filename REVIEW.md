# odatabase review

Reviewed at `de3e35e` (main, 2026-11-12). Scope: the seven crates under `crates/`, the test
suite, CI and the repository documents.

## Verdict

The project is in good shape for a first release, with one blocker: **access control does not
hold on inherited types.** Two of the gaps below let a caller read data the model says is
hidden, and one lets a tenant change another tenant's rows. All five numbered security findings
were reproduced with real HTTP requests against SQLite; the rest come from reading the code and
are marked as such.

Everything else is solid: the layering is clean, SQL is built only from model identifiers and
bound parameters, the test suite is broad and passes on both databases, and the README is
honest about what is not implemented.

## What was checked

| Check | Result |
|---|---|
| `cargo build --workspace --all-targets` | Clean |
| `cargo test --workspace` | 216 passed, 0 failed, 4 doctests ignored. Docker was running, so the PostgreSQL scenarios ran |
| `cargo clippy --workspace --all-targets` | No warnings |
| `cargo check -p odatabase --no-default-features` with `sqlite`, `postgres`, `sqlite,axum`, and no features | All compile without warnings |
| Not run | `cargo fmt --check`, `cargo doc`, `cargo deny`, the conformance clients, the load test |

Code read in full: `odatabase-sql` (`filter.rs`, `query.rs`, `context.rs`, `db.rs`, `values.rs`,
`search.rs`, `error.rs`), `odatabase-core` (`service.rs`, `request.rs`, `batch.rs`, the first
half of `read.rs`, the dispatch, create, update and upsert paths of `write.rs`),
`odatabase-axum`, and model validation in `odatabase-edm/src/builder.rs`. Not read closely:
`aggregate.rs`, `expand.rs`, `delta.rs`, the URI parser, the macros, and the rest of
`write.rs`. Findings in those areas would be additional to this report.

## Security findings

### 1. Joined inheritance ignores the tenant — cross-tenant read and write (critical)

The README says a key "only has to be unique within a tenant". With joined inheritance that
is not safe: the derived table is joined on the key columns alone.

Reproduction, with `asset (tenant_id, id)` as the tenant-scoped base and `machine (tenant_id,
asset_id, power)` joined on `id = asset_id`. Tenant `a` owns plain asset 1; tenant `b` owns
asset 1 with a machine row of power 9000.

| Request as tenant `a` | Result |
|---|---|
| `GET Assets` | `{"@odata.type":"#P.Machine","Id":1,"Name":"a-plain","Power":9000}` — tenant `b`'s row |
| `GET Assets/P.Machine` | Same row, 200 |
| `PATCH Assets(1)/P.Machine {"Power":1}` | 204, and tenant `b` then reads `Power: 1` |

Where: [filter.rs:162-178](crates/odatabase-sql/src/filter.rs:162) (`enter` joins on `join`
only), [query.rs:293-308](crates/odatabase-sql/src/query.rs:293) (`select_entity` left-joins
derived tables the same way), and
[write.rs:2560-2573](crates/odatabase-core/src/write.rs:2560) (`table_conditions` keeps only
the join columns for a joined table, dropping the tenant condition).
`collection(...)` and `join_table(...)` have a `tenant_column` option for exactly this case;
`joined(...)` has none.

Fix: give `joined(...)` a `tenant_column` and add it to every join, update and delete on the
derived table. Until then, make `build` reject a joined subtype of a tenant-scoped type, so
the unsafe combination cannot be deployed. Services whose keys are globally unique are not
affected.

### 2. `visible_for` on a derived type is not enforced through its base (high)

A type restricted to a group is still returned, with its own properties, when read through a
visible base type. Reproduction: `Snake` derives from `Animal` and is `visible_for("admin")`;
the caller has no groups.

| Request | Result |
|---|---|
| `GET Animals` | 200, includes `{"@odata.type":"#P.Snake",…,"Venom":"neurotoxin"}` |
| `GET Animals?$filter=P.Snake/Venom eq 'neurotoxin'` | 200, filters on the hidden property |
| `GET Animals?$select=P.Snake/Venom` | 200, returns `Venom` |
| `GET Animals/P.Snake`, `GET Animals(2)/P.Snake/Venom` | 403, as intended |

Where: [query.rs:291-331](crates/odatabase-sql/src/query.rs:291) selects the properties of
every derived type and checks only property-level visibility;
[filter.rs:988-1010](crates/odatabase-sql/src/filter.rs:988) (`Step::Cast`) never calls
`claims.sees` on the cast target. `$metadata` does hide the type, so the data and the
metadata disagree.

Fix: in `select_entity`, skip derived types the caller cannot see, and decide whether their
rows are dropped or shown as the base type; check type visibility at every cast step in
expressions and in `$select`. Add a security test for each path, as
`protection_follows_every_path` does for row protection.

### 3. A function hidden with `visible_for` can be called from `$filter` (medium)

`GET Hidden(x=-1)` answers 403, but `GET Animals?$filter=P.Hidden(x=Id) eq 1` answers 200 and
runs the database function. [filter.rs:1527-1565](crates/odatabase-sql/src/filter.rs:1527)
resolves the overload without checking `op.visible_for`; the check exists only on direct calls
in `operation.rs`. Fix: check `claims.sees(&op.visible_for)` in `Translator::function`.

### 4. `outerjoin` through a join table counts links to hidden rows (low to medium)

A group linked to three documents, of which the caller may see one:

| Request | `N` |
|---|---|
| `$apply=join(Docs as D)/aggregate($count as N)` | 1 |
| `$expand=Docs/$count` | 1 |
| `$apply=outerjoin(Docs as D)/aggregate($count as N)` | **3** |

The link table is left-joined first, then the target with its restrictions in `ON`, so each
link to a hidden row survives as a row of nulls
([filter.rs:654-693](crates/odatabase-sql/src/filter.rs:654)). The existing test covers the
foreign-key case, where this cannot happen. Fix: join the link table and target as one nested
join, or keep link rows only where the target matched or no link exists.

### 5. `_` and `%` in a claim value are wildcards when `wildcards` is set (low)

With `protected(claim = "region", wildcards)` and the claim value `eu_west`, the caller sees
rows with region `eu_west`, `eu-west` and `euXwest`.
[filter.rs:364-373](crates/odatabase-sql/src/filter.rs:364) passes `_` and `%` through to
`LIKE` unescaped. This matches the Java project's behaviour, but the macro documentation
mentions only that wildcards exist, and underscores are common in identifiers. Either escape
`%` and `_` and keep `*` and `+` as the wildcard characters, or document all four.

## Other findings, from reading the code

Not reproduced; each is a reading of the cited lines.

**Protocol and robustness**

- **Fixed multipart boundary.** `$batch` responses always use `batch_response`
  ([batch.rs:534-535](crates/odatabase-core/src/batch.rs:534)). JSON bodies cannot contain a
  raw line break, but `$value` responses can, so stored text containing
  `\r\n--batch_response--` would let one user truncate or forge parts of a batch response
  another client parses. Generate a random boundary per response.
- **Content-ID substitution is textual.** `substitute_body` replaces `"$1"` anywhere in the
  body ([batch.rs:267-277](crates/odatabase-core/src/batch.rs:267)), so a string property whose
  value is literally `$1` is rewritten to a URL. Substitute only in `@odata.bind` and `@id`
  values after parsing.
- **Batch parts skip two checks.** `run_inner`
  ([batch.rs:156-171](crates/odatabase-core/src/batch.rs:156)) does not apply the
  `$schemaversion` and `OData-MaxVersion` checks that `dispatch` applies to top-level
  requests.
- **Service root comes from request headers.** `Host` and `X-Forwarded-Proto` are trusted
  unconditionally ([odatabase-axum/src/lib.rs:59-69](crates/odatabase-axum/src/lib.rs:59)) and
  end up in `@odata.nextLink`, `Location` and `@odata.context`. The README says to run behind
  a proxy; a `Config` option for a fixed public root would remove the dependency and the
  cache-poisoning risk when the proxy forwards `Host` unchanged.
- **Regular expressions run in the database.** `matchesPattern` passes the client's pattern
  to PostgreSQL `~` ([filter.rs:1435](crates/odatabase-sql/src/filter.rs:1435)). Only
  `statement_timeout` bounds it, and that is set only by `Database::connect_postgres`, not on
  a pool the application builds itself. Consider a capability switch to turn the function off.
- **Hard-coded limits.** The 16 MB body limit and the 1000-request batch limit are constants.
  There is no limit on the number of `in` items, `$orderby` terms or `$select` paths, only on
  expression depth. Make them `Config` fields.

**Performance**

- **Collection properties are matched quadratically.**
  [query.rs:983-1001](crates/odatabase-sql/src/query.rs:983) scans every loaded item for every
  parent record, and applies the item-level `$top` and `$skip` in memory after loading all
  items of all parents. At the default page size of 1000 this is a million comparisons per
  collection property per page. Group the items by owner key once.
- **`$metadata` is rebuilt on every request.**
  [read.rs:189-215](crates/odatabase-core/src/read.rs:189) clones and filters the model,
  serializes it and hashes the result each time, including for requests that end in a 304.
  Cache the document and tag for the unrestricted model, and per group set otherwise.
- **Responses are buffered.** Already documented in the README; a batch of 1000 reads at 1000
  rows each is held in memory at once.

**Maintainability**

- [write.rs](crates/odatabase-core/src/write.rs) is 2802 lines with several
  `#[allow(clippy::too_many_arguments)]`. Splitting it into insert, update, links, collections
  and media, with a context struct for `req`, `session` and `type_name`, would make the write
  rules reviewable. Finding 1 lives partly here.
- The security model depends on every caller of the translator remembering each check. `enter`
  is a good single choke point for rows; type and operation visibility have no equivalent,
  which is how findings 2 and 3 arose. One function that resolves a type, navigation, property
  or operation "as seen by these claims" would close the class.
- `validate` in [builder.rs:247-352](crates/odatabase-edm/src/builder.rs:247) checks names and
  keys but no cross-feature combinations. Finding 1 is a combination that should fail at
  `build`.

**Testing and CI**

- Nearly all tests are HTTP scenarios in `crates/odatabase/tests`, which is the right
  emphasis. The hand-written parsers of untrusted input — the URI lexer and parser and the
  multipart reader — have no fuzz or property tests, and the derive macros have no
  compile-fail tests for their error messages.
- CI builds default features only. The reduced feature sets compile today, but nothing keeps
  them compiling. Add a small feature matrix to the `msrv` or `test` job.
- Workflow actions are pinned by tag rather than commit SHA. For a library whose pitch
  includes security, pin by SHA.

## Strengths

- **SQL safety.** `unsafe` is forbidden workspace-wide. Identifiers come from the model and
  every literal is a bound parameter; I found no place where request text reaches a SQL
  string. Database errors are mapped to generic messages and logged server-side.
- **Row restrictions have one entry point.** Tenant, claim and query-extension conditions are
  applied in `Translator::enter` and its two join variants, and the tests walk every path to
  a protected type: navigation, `$expand`, lambdas, `$count`, `$apply`, `$batch`.
- **Tests.** 216 tests, each scenario on SQLite and again on PostgreSQL, with SQL and
  `$metadata` snapshots, an end-to-end test over a real socket, and five third-party clients
  in CI. A missing Docker daemon fails CI rather than silently skipping.
- **Documentation.** The README states defaults, limits and unsupported features, and the
  code follows its own rule of answering 501 or 400 rather than a wrong result.
- **Dependency hygiene.** `cargo deny` covers advisories, licenses and sources, with the one
  exception explained.

## Recommended order

1. Fix findings 1 and 2 before any release, each with a regression test on both databases.
2. Fix 3 and 4 in the same pass, and settle 5 by escaping or documenting.
3. Random batch boundary and structured Content-ID substitution.
4. A fixed public root in `Config`, and the hard-coded limits as `Config` fields.
5. Group collection items by owner; cache `$metadata`.
6. Fuzz the URI and multipart parsers; add the CI feature matrix.
7. Split `write.rs`.

## Status

All five security findings and nearly all other findings were fixed before the first release,
[0.1.0](CHANGELOG.md):

- Findings 1 to 5, the batch boundary, Content-ID substitution and the batch part checks in
  [#18](https://github.com/danfengtong19983/odatabase/pull/18). Joined subtypes take a `tenant_column`, derived types and functions are checked
  against `visible_for` on every path, `outerjoin` keeps only links whose target matched, and
  `_` and `%` in claim values are escaped.
- The public root, the limits, the `matchesPattern` switch, collection paging, the
  `$metadata` cache, the parser and macro tests, the CI feature matrix, action pinning and the
  `write.rs` split in [#19](https://github.com/danfengtong19983/odatabase/pull/19).

Still open: the context struct for the write path, and a single "as seen by these claims"
resolver for types and operations. Responses are still buffered, as the README says.

Findings 1 to 4 are vulnerabilities by the definition in `SECURITY.md`. They were kept out of
the public repository until the fixes were released.

Line links point to the reviewed commit `de3e35e`; `write.rs` has since been split into
`crates/odatabase-core/src/write/`.
