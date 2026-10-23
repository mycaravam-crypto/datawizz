# datawizz

Small, fast, local-first PostgreSQL UI written in Rust. One executable; no Node or frontend build.

## MVP features
- Explore readable PostgreSQL tables, views and columns
- Execute a single SQL statement (SELECT and permitted writes/DDL)
- Display bounded results, errors, runtime and affected rows; export **loaded rows only** as CSV
- Built-in HTML/JavaScript editor. No user accounts or stored credentials.

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

Place any required **PEM-encoded** corporate root/intermediate CA certificates in `certs/`, using `.crt` or `.pem` extensions. Multiple files are supported. The certificates are mounted only in a Docker BuildKit build step and included in the builder trust store, **not in the final runtime image**. The directory is ignored by Git except for `certs/.gitkeep`.

Build with either command (no extra secret or override configuration required):
```sh
docker build -t datawizz:local .
docker compose up --build
```

If certificates are installed or replaced, use `docker compose build --no-cache app` to force the CA setup to run again. This is for TLS interception when downloading Rust crates; PostgreSQL traffic still uses `NoTls`. Never disable certificate verification or commit your certificates.

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

## Configuration

| Environment | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | required | PostgreSQL URL |
| `HOST` | `127.0.0.1`; Docker sets `0.0.0.0` | HTTP interface |
| `PORT` | `3000` | HTTP port |
| `MAX_ROWS` | `200` (max `1000`) | UI row limit |
| `QUERY_TIMEOUT_MS` | `10000` (max `60000`) | PostgreSQL statement timeout |

## Security and limitations

**Single-user development use only.** HTTP has no authentication; never publish it publicly. The application executes arbitrary SQL with the configured database role's privileges. Use a minimally privileged role and a trusted database.

The current PostgreSQL driver uses `NoTls`: **TLS connections are not implemented yet**. Do not use it across untrusted networks. The row limit caps response memory, not the work performed inside PostgreSQL: extra rows are consumed and discarded until completion or timeout. The application opens a DB connection per request. It does not implement profiles, multi-statement scripts, autocomplete, query history, a GUI row editor, or advanced administration.

The CI Docker build passed on the initial MVP. The new Compose PostgreSQL smoke test still requires local verification. Follow issues #1-#6.
