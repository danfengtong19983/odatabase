# Security policy

odatabase builds SQL from requests and enforces row protection, tenancy and visibility, so a bug
can expose or change data that a caller shouldn't reach. Please report such problems privately.

## Reporting a vulnerability

Use [Report a vulnerability](https://github.com/danfengtong19983/odatabase/security/advisories/new)
on the repository's Security tab. Don't open a public issue.

Include the model (the derived types or a reduced version), the request, and what it exposed or
changed. Once a fix is released, the advisory is published with credit to you unless you prefer
otherwise.

## What counts

- SQL injection, or SQL built from text the model didn't supply.
- Reading or changing rows that row protection, a tenant claim, a query extension or
  `visible_for` should hide, through any path: navigation, `$expand`, `$filter`, lambdas,
  `$apply`, `$batch`, `$crossjoin`, `$all`, counts, delta links or error messages.
- Database error details reaching clients.
- Requests that bypass the configured limits on expression depth, expand depth or page size.

Out of scope: authentication itself. The library trusts the `Claims` your middleware attaches
to a request.

## Supported versions

Before 1.0, only the latest minor release receives fixes.
