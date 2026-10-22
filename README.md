# datawizz

Small, fast, local-first PostgreSQL UI written in Rust. One executable; no Node or frontend build.

## MVP features
- Explore readable PostgreSQL tables, views and columns
- Execute a single SQL statement (SELECT and permitted writes/DDL)
- Display bounded results, errors, runtime and affected rows; export **loaded rows only** as CSV
- Built-in HTML/JavaScript editor. No user accounts or stored credentials.

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

This is the first implementation increment; a Docker compilation and live PostgreSQL integration run have not yet been verified. Follow issues #1-#6.
