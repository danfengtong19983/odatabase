# Client conformance

Runs OData clients written by other projects against
[conformance_server](../crates/odatabase/examples/conformance_server.rs), so the server is
checked by code that doesn't share its assumptions. Each script starts a fresh in-memory server
and runs the client in Docker. They aren't part of `cargo test`, but CI runs them on every change.
A script's exit code is the client's result.

| Script | Client | What it checks |
|---|---|---|
| `run-dotnet.sh` | Microsoft.OData.Core 8 | Strict `$metadata` and payload validation, context URLs, multipart `$batch`, `$apply` grammar |
| `run-java.sh` | Apache Olingo client 5 | Its own URL encoding, CRUD, ETags, operations, `$batch` |
| `run-ui5.sh` | SAPUI5 OData V4 model in headless Chromium | Metadata model, `$batch` traffic, updates with `If-Match`, operations, `$apply` from `$$aggregation` |
| `run-python.sh` | python-odata | Reflection from `$metadata`, queries, writes |
| `run-openapi.sh` | OASIS odata-openapi | Conversion of `$metadata` to OpenAPI |

```bash
for s in dotnet java ui5 python openapi; do conformance/run-$s.sh; done
```

`run-ui5.sh` needs internet access and always uses the current OpenUI5 release, because
`sdk.openui5.org` removes old ones. A new release can change its result, so its CI job doesn't
block a change. The other client versions are pinned. On Linux, set `BIND=0.0.0.0` so that
containers can reach the server.

## Client behavior the tests work around

- **Olingo** doesn't double quotes in key values, so `appendKeySegment("O'Neil")` sends an invalid
  URL. The server answers 400, which is correct. Pass `O''Neil`.
- **UI5** rejects `$top` inside `$expand` of a collection before sending a request.
- **python-odata** names reflected entities after their sets, such as `BusinessPartners`.
