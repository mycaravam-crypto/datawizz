# datawizz

Small, fast, local-first PostgreSQL UI written in Rust. One runtime executable; CodeMirror 6 is bundled at Docker/CI build time, with a no-build textarea fallback for plain Cargo builds.

## MVP features
- Explore readable PostgreSQL tables, views and columns
- Optional server-defined connection profiles with safe read-only mode and an ER relationship view
- Execute a single SQL statement (SELECT and permitted writes/DDL)
- Display bounded results, errors, runtime and affected rows; export loaded rows or up to 10,000 server-filtered rows / 10 MiB as a snapshot-consistent CSV
- Offline-bundled CodeMirror 6 PostgreSQL SQL editor with highlighting, line numbers, completion and keyboard search; a textarea fallback remains available. No user accounts or stored DB credentials.
- PostgreSQL administration inventory: databases, roles/memberships, sessions, extensions, schemas, relations, indexes, constraints, table statistics, locks, safe server settings, visible table privileges and routines (max 100 entries per section).
- Guided administration SQL templates for roles, memberships, schemas, table/database grants, database lifecycle, session control and maintenance; generated statements open in a separate editor tab and never run automatically.
- SQL productivity: bounded session-only query history, manually saved browser scripts, and Ctrl+Space completion from loaded PostgreSQL metadata.

## Quick start: disposable test PostgreSQL (recommended)

Only Docker Desktop / Docker Compose is required; no local Cargo or PostgreSQL.

```sh
docker compose up --build
```

Open **http://127.0.0.1:3000**. The browser can explore `public.products` and `public.available_products`. Example query:

```sql
SELECT * FROM public.products;
```

The test DB is intentionally disposable (tmpfs). All changes are lost when the PostgreSQL container stops/restarts. It is reachable only from the Compose network; PostgreSQL port 5432 is **not** published on the host. The app's port is published only to host loopback. These static test credentials are for local development only.

Stop and remove the containers:
```sh
docker compose down
```

## Always use the current UI during Compose development

The workbench HTML is embedded in the Rust executable at compile time (`include_str!`).
Refreshing the browser alone does not load a changed `web/index.html` from disk.
For automatic rebuilding while developing (Docker Compose 2.22+), start:

```sh
docker compose up --build --watch
```

Compose Watch observes `web/`, `src/`, `Cargo.toml`, and `Dockerfile`.
Every relevant edit triggers an image rebuild and replacement of the app container.
The PostgreSQL container is not forcibly recreated. Because Rust embeds HTML,
`action: rebuild` is required; a file sync or container restart is insufficient.

For a one-time build and container refresh, use `docker compose up -d --build`.
For an explicit **byte-for-byte verification** that the browser endpoint serves
the current source (including detection of a wrong port/container), run:

```sh
bash scripts/refresh-ui.sh
```

This runs `docker compose up -d --build --force-recreate app` and compares the
actual HTML served at `http://127.0.0.1:3000/` byte-for-byte with local
`web/index.html`. A mismatch exits with an error rather than silently showing
an old UI. The script requires Docker Compose, curl, cmp, and Bash.
To verify another address, set `DATAWIZZ_URL`.

To **check the already running application without rebuilding**, run:

```sh
bash scripts/refresh-ui.sh --verify-only
```

This detects an old running container immediately after a UI edit and does not modify containers. A mismatch causes a nonzero exit status.

The HTTP response already uses `Cache-Control: no-store`. If the UI still appears
old, check the URL/port and whether another Compose project or local process
is serving it. There is no runtime HTML bind mount in the release image.

## Corporate TLS interception during Docker build

Place any required **PEM-encoded** corporate root/intermediate CA certificates in `certs/`, using `.crt` or `.pem` extensions. Multiple files are supported. Docker BuildKit mounts these certificates only during dependency setup: they are passed to npm through `NODE_EXTRA_CA_CERTS` in the Node editor builder and installed in the Rust builder trust store. They are **not copied into the final runtime image**. The directory is ignored by Git except for `certs/.gitkeep`.

Build with either command (no extra secret or override configuration required):
```sh
docker build -t datawizz:local .
docker compose up --build
```

If certificates are installed or replaced, use `docker compose build --no-cache --progress=plain app` to rerun both dependency steps. This is for TLS interception when downloading npm packages and Rust crates; it does not change the database connection's TLS mode. Never disable certificate verification or commit your certificates.

The editor dependencies are locked in `package-lock.json` and installed with `npm ci`. A BuildKit cache mount reuses downloaded npm packages across local builds, including when application source files change. For diagnosing slow downloads, use `docker compose build --progress=plain app` to see the complete npm output; on a fresh host the registry must still be reachable.

## Build using Docker (no local Cargo)

```sh
docker build -t datawizz:local .
```

**Docker Desktop (macOS/Windows), PostgreSQL on host:**
```sh
docker run --rm -p 127.0.0.1:3000:3000 \
  -e DATABASE_URL='postgres://user:password@host.docker.internal:5432/database' \
  datawizz:local
```

**Linux, PostgreSQL on host:**
```sh
docker run --rm --network host -e HOST=127.0.0.1 \
  -e DATABASE_URL='postgres://user:password@127.0.0.1:5432/database' \
  datawizz:local
```

Open http://127.0.0.1:3000

On Linux using Docker bridge networking instead of host networking, publish `-p 127.0.0.1:3000:3000` and supply a database hostname or IP reachable from the container.

The multi-stage Dockerfile compiles in the official Rust builder and copies the release binary into a Debian runtime image. No local Rust or Cargo installation is required.

For compiler and test checks through Docker:
```sh
docker run --rm -v "${PWD}:/work" -w /work rust:1-bookworm cargo check
docker run --rm -v "${PWD}:/work" -w /work rust:1-bookworm cargo test
```

## PostgreSQL administration

The **Administration** workspace tab uses a database-tool layout inspired by
DataGrip: a category explorer on the left, the selected catalog as a full-width
table on the right, and a detail inspector for the selected row. Switch back to
**SQL console** using the workspace tabs. The administration explorer groups
**Server objects**, **Database objects**, **Monitoring** and **Tools**. Select
a category, choose **Load metadata**, and optionally filter the rows of that
category. Only one catalog table is displayed at a time; the layout stacks
on narrow screens.

The administration view loads a read-only catalog snapshot on demand:
database and role metadata, memberships, sessions (without SQL text or client
addresses), extensions, schemas, relations and sizes, index validity/usage,
constraints, table scan/vacuum statistics, relation locks, safe server settings,
visible table grants and routine signatures. The detail inspector also shows
PostgreSQL's actual index definitions, view SELECT definitions and constraint
definitions, where applicable; **Open DDL in SQL console** only creates an
editor draft. It does not execute the definition. It is not a complete CREATE
TABLE reverse-engineering feature. Each category returns at most 100
items. Fixed read-only PostgreSQL catalog queries never expose password
hashes or arbitrary configuration values.

The **SQL templates** item under Administration → Tools prepares one PostgreSQL statement at a time
for common tasks:

- Roles: create (NOLOGIN), drop, enable/disable login; grant/revoke membership.
- Objects: create/drop/rename tables, add/drop/rename/retarget columns with a strict type allowlist, create/drop indexes, create/drop views and UNIQUE constraints, table ownership. SQL preview, no autoexecution.
- Schemas, tables and databases: create/drop schema or database; grant/revoke
  schema, table or database privileges.
- Operations: analyze, vacuum, reindex, cancel a query, terminate a session,
  create/drop an extension.
- Additional reviewed DDL: same-schema foreign keys, NOT NULL, literal column
  defaults, schema ownership/rename, view/index rename and table/column comments.

Identifiers are quoted, privileges selected from fixed allowlists, and process
IDs validated. The generated SQL is **never executed or posted by the builder**.
It opens in a new SQL tab, preserving previous drafts. Destructive templates
require a confirmation before opening; executing a statement still requires an
explicit click on **Run query**. PostgreSQL privileges govern execution.
Some commands (for example DROP DATABASE) cannot be run against the active
database, and DB permissions/locks may reject them.

**Security limitation:** This remains a **single-user development application**.
A bearer token and verified PostgreSQL TLS are available by explicit
configuration, but the default Docker environment is for trusted local use only.
Do not expose plain HTTP or database connections to untrusted networks. The SQL editor already supports arbitrary permitted SQL. The SQL
builder does not bypass or elevate database rights. Direct write-capable admin
endpoints, server configuration changes and remote operation remain blocked
pending the security prerequisites in
[the administration roadmap](https://github.com/mycaravam-crypto/datawizz/issues/59).

## Optional API access token and verified PostgreSQL TLS

The default loopback-only development mode remains unchanged. Set
`DATAWIZZ_ACCESS_TOKEN` (at least 32 characters) to protect **all API routes**.
The browser asks for the token on the first unauthorized response, keeps it only
in memory and sends it via an `Authorization: Bearer` header. It is never
stored in localStorage or in URL parameters. This is **not a public deployment
mode**: a token over plain HTTP can be intercepted. Use a trusted local host,
or provide HTTPS and access control in an external reverse proxy before any
remote exposure; never directly publish the service.

Set `DATABASE_TLS=verify-full` to require a certificate-verified PostgreSQL
TLS connection. The system trust store is used; optionally supply a PEM trust
anchor at `DATABASE_TLS_CA_FILE`. Hostname verification remains enabled and
there is no insecure skip-verification mode. `DATABASE_TLS=disable` retains
the original local development default. Use TLS for external DB hosts.

Connection switching, session isolation, SSH tunneling and user-specific DB
permissions remain outside this first security increment.

## Database Workspace profiles and ER relationships (#72)

The **Connection** selector can switch between separately configured PostgreSQL
profiles. Every profile has immutable server-side routing: a request for
`/profiles/<id>/api/...` uses that profile's own connection settings, while
`/api/...` remains the default database for compatibility. The browser
receives profile **id, label, environment and read-only flag**, never a
connection URL, database password or TLS trust file. The existing API bearer
token protects all profile routes.

Optional profile configuration (`DATABASE_PROFILES_JSON`, maximum seven
additional profiles) is a JSON array:

```sh
export DATAWIZZ_ACCESS_TOKEN='use-at-least-32-random-characters-here'
export DATABASE_PROFILES_JSON='[
  {"id":"staging","label":"Staging","environment":"test",
   "url":"postgres://readonly_user:password@db:5432/staging",
   "read_only":true,"tls":"verify-full"}
]'
```

These values belong to the **server's secret configuration**, not browser
storage, a Git repository, logs or client-visible JavaScript. Do not use these
example credentials. Additional profiles require `DATAWIZZ_ACCESS_TOKEN`
and are otherwise rejected at startup. Valid ids contain only lowercase
ASCII letters, digits, hyphens or underscores. Environments are `dev`,
`test` and `prod`. New profiles default to **read-only**. Production
profiles **must** enable read-only and `tls: "verify-full"`. An optional
`ca_file` points to a local PEM CA. TLS hostname verification remains enabled.

Read-only profiles start each request in a PostgreSQL `READ ONLY` transaction,
and the dedicated row mutation APIs fail with HTTP 403. **A database role
with only SELECT privileges is still mandatory** for production: server-side
read-only mode is defense-in-depth, not a replacement for least privilege.
The initial default `DATABASE_URL` remains writable for local compatibility.
Switching profiles confirms loss of unsaved edits, then reloads the page.
Only the nonsensitive selected **profile id** is kept in tab-scoped
`sessionStorage`; access tokens, query history, loaded rows and editor tabs
are not persisted across switches. Previously saved scripts remain explicitly
saved, but are keyed by profile id. Switching requires entering the API token
again because it is only retained in memory.

The selector also checks the server-side database name/role. Administration
→ Tools → **ER relationships** renders a bounded, read-only graph based on
foreign-key metadata (at most 80 constraints and 40 graph nodes) and an
accessible text table. Only relationships whose source and target tables are
both readable to the active PostgreSQL role are returned. The graph does not
generate or execute SQL.

**Limits:** No SSH tunnels, arbitrary user-created runtime profiles, writable
production profiles, cross-profile query execution, consistent sessions across
requests, or interactive drag-and-drop schema modeling. Use HTTPS and
external access control before exposing this application outside trusted
loopback networking. The default Docker Compose setup remains local-only.

## SQL Editor 2.0 (Issue #70)

The default Docker/CI build embeds **CodeMirror 6** with PostgreSQL syntax
highlighting, line numbers, search (`Ctrl/⌘+F`), indentation and
`Ctrl/⌘+Space` metadata suggestions. There is **no runtime CDN dependency**;
the editor bundle is served by the existing Rust binary. Existing SQL tabs,
query history, admin-generated SQL drafts and `Ctrl/⌘+Enter` execution use
the same editor content. Completion reads only already loaded database metadata.

When running `cargo run` directly from a fresh source checkout, the minimal
built-in textarea fallback remains available. To enable the rich editor for
local Rust builds, install Node.js 22 and run:

```sh
npm install --no-audit --no-fund
npm run build:editor
cargo run
```

The Dockerfile performs the editor bundling automatically in a Node build
stage and copies only the final JavaScript bundle into the Rust build stage.
Node.js is not needed in the runtime image. For troubleshooting or
assistive-technology compatibility, append `?basic-editor=1` to use the
original textarea. CodeMirror is MIT-licensed; JavaScript package versions
are pinned in `package.json`.

**Optional draft recovery:** Query text and unsent tabs are never persisted
automatically unless the user explicitly enables **Save unsent SQL drafts**
and confirms the warning. Consent and drafts are browser-local, unencrypted
and profile-scoped. Limits: 12 tabs, 16 KiB each, 40 KiB total.
The user must explicitly click **Restore saved drafts** after a reload; SQL
is never executed by restoration. Disabling consent or choosing **Delete
saved drafts** removes both the stored drafts and consent. Never enable this
on shared workstations or for confidential SQL.

Saved scripts now have an explicit **Export** action to download a `.sql` file.
Query history remains memory-only and is never included in automatic
recovery.

## SQL productivity (first increment of #70)

The SQL console includes a collapsible **Query history & saved scripts** panel.

- **History** records the last 40 attempted executions (success/failure, execution time,
  SQL and available duration). It is searchable, kept **only in memory** and cleared
  when the page is refreshed or closed. Opening a history entry creates a new editor
  tab; it does not re-run the statement. Statements over 16,000 characters are
  deliberately not saved as incomplete history entries.
- **Saved scripts** are stored **only when you explicitly choose Save locally and
  confirm**. Up to 20 named scripts (16 KiB each) are kept in this browser's
  unencrypted `localStorage` at the application origin. They remain accessible
  to anyone using that browser profile and may be visible when a different database
  is configured at the same origin. Do **not** store passwords, tokens or confidential
  SQL. Scripts can be reopened in a new tab or deleted, neither action executes SQL.
- **SQL completion:** Press `Ctrl+Space` or `Cmd+Space` within the existing
  editor to get SQL keyword and visible schema/table/column suggestions. Use arrows
  and Enter/Tab or click a suggestion; Escape dismisses it. Suggestions are drawn
  only from metadata already loaded by the normal explorer, without a new API call.

The rich editor is bundled for Docker and CI. The textarea remains as the
explicit basic-editor fallback; no CDN, implicit persistence or browser extension
is required. Accessible editor ergonomics and completion can be further refined
under #70.

## Data Explorer 2.0 — database filters, ordering and bounded export

Select a table or view, then use **Filter column**, **Contains**, **Sort by**
and **Direction** above the grid. Click **Apply to database**, or click a table
column heading to change server-side ordering. Results are fetched from
PostgreSQL, not just filtered within the loaded 100-row page. The API accepts
only catalog-validated column names, `asc`/`desc` directions and bound filter
values. Filters use **case-insensitive literal substring** matching against
PostgreSQL's text representation (including numbers and dates); `%`, `_` and
`#` are literal characters, not wildcards. SQL NULL does not match. Paging
uses a primary-key tie-breaker if the table has one. Without a primary key,
rows can shift across pages during concurrent updates.

Use **Export filtered CSV** to download up to **10,000 matching rows / 10 MiB**
from one PostgreSQL read-only transaction snapshot. Export respects the active
server-side table filter and sort; values are CSV-escaped and spreadsheet
formula prefixes are neutralized. SQL NULL and an empty string remain distinct.
The server rejects larger exports with HTTP 413 instead of silently truncating
CSV content. **Cancel export** aborts the request and prevents saving a partial
file. Staged but unsaved table edits are not exported. The existing **Export
loaded CSV** exports only the currently loaded SQL result page.

The export uses the configured bearer-token authentication and selected
database profile; the token is sent in an HTTP header, never in a URL. The
server does not stream arbitrarily large exports: exports above the stated
limits, multi-column predicates and bulk CSV import with mapping remain open in
[Data Explorer 2.0 issue #71](https://github.com/mycaravam-crypto/datawizz/issues/71).

## Bounded CSV import and EXPLAIN preview

Administration → Tools → **CSV import preview** reads a UTF-8 CSV locally (maximum
16 KiB, 100 data rows and 20 columns), matches its header to readable PostgreSQL
table columns, and generates **one atomic INSERT statement** for manual review.
Identifiers and values are safely quoted. Empty strings remain empty by
default, with explicit opt-in for SQL NULL. Import does not execute or upload
anything until **Run query** is clicked in the SQL console. This initial tool
does not perform arbitrary-sized bulk loading, background jobs, schema
migration, or automatic backup/restore.

Use **Explain SQL** in the SQL console to prepare `EXPLAIN (FORMAT JSON)` in
a separate tab. The plan is not requested until you explicitly click Run query.
Returned plans appear as a bounded, expandable **node tree**, including
node type, relation, estimated rows and costs; the raw JSON result remains
visible. The tool deliberately does not use `ANALYZE`, because that would
actually execute the underlying statement. Rich graphical plan tooling,
schema diff and safe native `pg_dump`/`pg_restore` workflows remain in #65.

## Configuration

| Environment | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | required | PostgreSQL URL |
| `HOST` | `127.0.0.1`; Docker sets `0.0.0.0` | HTTP interface |
| `PORT` | `3000` | HTTP port |
| `MAX_ROWS` | `200` (max `1000`) | UI row limit |
| `QUERY_TIMEOUT_MS` | `10000` (max `60000`) | PostgreSQL statement timeout |
| `DATAWIZZ_ACCESS_TOKEN` | unset | Optional API bearer token (at least 32 characters); protects all `/api/` routes |
| `DATABASE_TLS` | `disable` | `disable` or `verify-full` (verified PostgreSQL TLS) |
| `DATABASE_TLS_CA_FILE` | unset | PEM CA certificate for the verified TLS connection |

## Security and limitations

**Single-user development use only.** HTTP has no authentication; never publish it publicly. The application executes arbitrary SQL with the configured database role's privileges. Use a minimally privileged role and a trusted database.

PostgreSQL TLS with certificate and hostname verification is opt-in via `DATABASE_TLS=verify-full`. The default `disable` is for local development only. Do not use plaintext database or HTTP connections across untrusted networks. The row limit caps response memory, not the work performed inside PostgreSQL: extra rows are consumed and discarded until completion or timeout. The application opens a DB connection per request. It does not implement multiple connection profiles, multi-statement scripts, autocomplete, query history, secure remote administration, backup/restore or write-capable administration dialogs. The catalog is read-only and SQL templates are manual-review-only.

The CI Docker build passed on the initial MVP. The new Compose PostgreSQL smoke test still requires local verification. Follow issues #1-#6.
