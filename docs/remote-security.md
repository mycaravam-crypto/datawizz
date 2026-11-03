# Remote deployment security baseline (Issue #63)

DataWizz is a local-first single-user application. Remote access is opt-in and MUST be limited to trusted administrators. This example does not add user accounts or make the application safe for arbitrary Internet exposure.

## Required architecture

1. Bind the Rust server to loopback (`HOST=127.0.0.1`, `PORT=3000`). Do not directly publish it.
2. Put the HTTPS reverse proxy on an authenticated, restricted network (VPN or allowlisted administrative network). Enforce strong authentication at the edge in addition to the DataWizz bearer token.
3. Supply a high-entropy `DATAWIZZ_ACCESS_TOKEN` securely at runtime, not through Git, query strings, browser storage or logs. Rotate it on compromise. The browser keeps it in memory.
4. Set `DATABASE_TLS=verify-full` (and a trusted `DATABASE_TLS_CA_FILE` if needed). The PostgreSQL hostname must match the certificate.
5. Use a dedicated PostgreSQL LOGIN role for each environment. Production credentials must have only required SELECT privileges, no schema creation, INSERT, UPDATE, DELETE or role elevation. Additional production profiles require both `read_only=true` and `tls=verify-full`.
6. Serve the site only from one HTTPS origin. The proxy must reject unexpected browser Origin values on API calls; do not enable cross-origin credentials or permissive CORS. Bearer authentication is required for both browser and API clients. Reject unexpected Host values.
7. Keep access and error logs free of Authorization headers, query strings, SQL values and connection credentials. Protect secrets, certificates and proxy configuration using filesystem permissions and access controls.

The example at `deploy/nginx/datawizz.conf` is a reference template and **allows loopback clients only** (for example through an administrator-controlled SSH tunnel). Keep this fail-closed rule until an approved VPN, edge identity check and explicit source-IP policy are in place. Replace the hostname and certificates before deployment. Validate with `nginx -t` and conduct an end-to-end TLS/authentication test. This template does **not** implement external user authentication or a VPN by itself.

## Authorization matrix

| Environment | PostgreSQL privilege | DataWizz profile | Transport |
| --- | --- | --- | --- |
| Local DEV | Dedicated disposable development role, optionally DML | Default local single-user profile | Loopback only |
| TEST | Restricted test credentials for each capability; no superuser in application | Explicit profile; read-only where appropriate | HTTPS when remote, PostgreSQL verify-full |
| PROD | SELECT on explicitly approved tables/views, USAGE on selected schemas, CONNECT on approved database | `environment=prod`, `read_only=true` | HTTPS edge plus PostgreSQL verify-full |

A database `GRANT SELECT` is the real privilege boundary. The application transaction-level read-only mode only adds defense in depth. The SQL editor can submit any SQL allowed by the connected DB role.

## Acceptance and negative cases

- Anonymous and invalid-bearer API requests must return 401 without secrets.
- Browser requests from disallowed origins must be rejected by the proxy; authorized same-origin calls still work.
- Database credentials with SELECT-only privileges may read approved tables but cannot change or inspect restricted tables.
- Untrusted/expired certificates, hostname mismatches and unavailable PostgreSQL servers fail closed without leaking DSNs/passwords.
- Misconfigured profiles or missing access tokens must fail startup or deny access rather than silently weaken controls.
- Rollback is to disable the remote listener and retain the existing loopback-only development setup.

This document is deployment guidance, **not** a claim of verified remote-production security. Verify the actual proxy, identity provider, database permissions, monitoring and certificate lifecycle in the target environment.
