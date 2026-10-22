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

## Corporate TLS interception during Docker build

The builder installs normal system CAs by default. If your company intercepts TLS and your Docker build cannot fetch crates.io, provide the **trusted PEM-encoded corporate CA** locally in `certs/company-ca.crt`. The `certs/` directory is excluded from the Docker build context and Git tracking should be prevented by a local `.git/info/exclude` entry (or your global gitignore).

For the standalone build:
```sh
docker build --secret id=corporate_ca,src=certs/company-ca.crt -t datawizz:local .
```

For Compose, set the optional BuildKit secret in `compose.override.yaml` (ignored locally via `.git/info/exclude`):
```yaml
services:
  app:
    build:
      secrets:
        - corporate_ca
secrets:
  corporate_ca:
    file: ./certs/company-ca.crt
```
Then run `docker compose up --build`.

Never disable TLS verification or commit corporate CA material unintentionally. The CA is mounted only during the builder step; it is **not** included in the final runtime image. PostgreSQL's current driver still uses `NoTls`.

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
