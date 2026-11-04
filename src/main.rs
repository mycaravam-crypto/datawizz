mod connection_manager;

use std::{env, sync::Arc, time::Instant};

use axum::{
    extract::{Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use futures_util::TryStreamExt;
use native_tls::{Certificate, TlsConnector};
use postgres_native_tls::MakeTlsConnector;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio_postgres::{NoTls, SimpleQueryMessage};

#[derive(Clone)]
struct AppState {
    url: Arc<String>,
    max_rows: usize,
    timeout_ms: u64,
    tls: Option<Arc<TlsConnector>>,
    access_token: Option<Arc<String>>,
    read_only: bool,
    profiles: Arc<Vec<ConnectionOption>>,
    managed: Option<Arc<connection_manager::ManagedStore>>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn postgres_error(error: &tokio_postgres::Error) -> (StatusCode, Json<ApiError>) {
    // Return a bounded PostgreSQL SQLSTATE, never server-supplied messages,
    // SQL fragments, object names or connection details.
    let code = error
        .code()
        .map(|code| code.code())
        .filter(|code| {
            code.len() == 5
                && code
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
        .unwrap_or("UNKNOWN");
    api_error(
        StatusCode::BAD_REQUEST,
        format!("PostgreSQL query failed (SQLSTATE: {code})"),
    )
}

// Keep database credentials and TLS diagnostics out of HTTP responses and logs.
fn connection_error() -> (StatusCode, Json<ApiError>) {
    api_error(StatusCode::BAD_GATEWAY, "PostgreSQL connection failed")
}

async fn connect(state: &AppState) -> Result<tokio_postgres::Client, (StatusCode, Json<ApiError>)> {
    // Per-request PostgreSQL connection; use verified TLS when explicitly configured.
    let client = if let Some(tls) = &state.tls {
        let (client, connection) =
            tokio_postgres::connect(&state.url, MakeTlsConnector::new((**tls).clone()))
                .await
                .map_err(|_| connection_error())?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
    } else {
        let (client, connection) = tokio_postgres::connect(&state.url, NoTls)
            .await
            .map_err(|_| connection_error())?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
    };
    client
        .batch_execute(&format!("SET statement_timeout = {}", state.timeout_ms))
        .await
        .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Could not configure query timeout"))?;
    // A server-selected read-only transaction is defense in depth for a profile.
    // The PostgreSQL role must independently be restricted for production use.
    if state.read_only {
        client.batch_execute("BEGIN READ ONLY").await.map_err(|_| {
            api_error(
                StatusCode::BAD_GATEWAY,
                "Cannot start read-only transaction",
            )
        })?;
    }
    Ok(client)
}

// A bearer header is explicit, unlike browser cookies; no ambient session for CSRF.
fn authorized(expected: Option<&str>, authorization: Option<&str>) -> bool {
    match expected {
        None => true, // local single-user deployment remains backward compatible
        Some(secret) => {
            let Some(provided) = authorization.and_then(|v| v.strip_prefix("Bearer ")) else {
                return false;
            };
            bool::from(provided.as_bytes().ct_eq(secret.as_bytes()))
        }
    }
}

async fn require_access_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> impl IntoResponse {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !authorized(state.access_token.as_deref().map(String::as_str), token) {
        return api_error(StatusCode::UNAUTHORIZED, "Authentication required").into_response();
    }
    next.run(request).await
}

fn verified_pg_tls(mode: &str, ca_path: Option<&str>) -> Result<Option<Arc<TlsConnector>>, String> {
    match mode {
        "disable" => {
            if ca_path.is_some() {
                return Err("DATABASE_TLS_CA_FILE requires DATABASE_TLS=verify-full".into());
            }
            Ok(None)
        }
        "verify-full" => {
            let mut builder = TlsConnector::builder();
            if let Some(path) = ca_path {
                let pem = std::fs::read(path).map_err(|_| "Cannot load DATABASE_TLS_CA_FILE")?;
                let certificate = Certificate::from_pem(&pem)
                    .map_err(|_| "Invalid DATABASE_TLS_CA_FILE certificate")?;
                builder.add_root_certificate(certificate);
            }
            let connector = builder
                .build()
                .map_err(|_| "Cannot initialize TLS trust store")?;
            Ok(Some(Arc::new(connector)))
        }
        _ => Err("DATABASE_TLS must be 'disable' or 'verify-full'".into()),
    }
}

#[derive(Debug, Clone, Serialize)]
struct ConnectionOption {
    id: String,
    label: String,
    engine: String,
    environment: String,
    read_only: bool,
}

#[derive(Deserialize)]
struct ExtraProfile {
    id: String,
    label: String,
    url: String,
    environment: String,
    #[serde(default = "default_read_only")]
    read_only: bool,
    tls: Option<String>,
    ca_file: Option<String>,
}
fn default_read_only() -> bool {
    true
}

fn configured_profiles(raw: &str) -> Result<Vec<ExtraProfile>, String> {
    let profiles: Vec<ExtraProfile> = serde_json::from_str(raw)
        .map_err(|_| "DATABASE_PROFILES_JSON must be an array of connection profiles")?;
    if profiles.len() > 7 {
        return Err("Maximum 7 additional profiles".into());
    }
    let mut ids = std::collections::HashSet::new();
    for p in &profiles {
        if p.id.is_empty()
            || p.id.len() > 32
            || !p
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            || p.id == "default"
            || !ids.insert(p.id.clone())
        {
            return Err(
                "Profile ids must be unique, lowercase a-z/0-9/-/_ and not 'default'".into(),
            );
        }
        if p.label.is_empty()
            || p.label.len() > 64
            || !p.label.chars().all(|c| !c.is_control())
            || p.label.contains("://")
            || p.label.contains('@')
        {
            return Err("Profile labels must not contain credentials or control characters".into());
        }
        if !matches!(p.environment.as_str(), "dev" | "test" | "prod") {
            return Err("Profile environment must be dev, test, or prod".into());
        }
        if p.url.is_empty() || p.url.len() > 4096 {
            return Err("Profile URL must be 1..4096 characters".into());
        }
        if p.environment == "prod" && (!p.read_only || p.tls.as_deref() != Some("verify-full")) {
            return Err("Production profiles require read_only=true and tls=verify-full".into());
        }
    }
    Ok(profiles)
}

async fn connections(State(state): State<AppState>) -> Json<Vec<ConnectionOption>> {
    let mut profiles = (*state.profiles).clone();
    if let Some(store) = &state.managed {
        profiles.extend(store.connection_options().await);
    }
    Json(profiles)
}

#[derive(Serialize)]
struct ConnectionStatus {
    database: String,
    role: String,
    read_only: bool,
}
async fn connection_status(State(state): State<AppState>) -> ApiResult<ConnectionStatus> {
    let client = connect(&state).await?;
    let row = client
        .query_one("SELECT current_database()::text, current_user::text", &[])
        .await
        .map_err(|_| connection_error())?;
    Ok(Json(ConnectionStatus {
        database: row.get(0),
        role: row.get(1),
        read_only: state.read_only,
    }))
}

#[derive(Serialize)]
struct Relationship {
    from_schema: String,
    from_table: String,
    to_schema: String,
    to_table: String,
    constraint: String,
    definition: String,
}
async fn relationships(State(state): State<AppState>) -> ApiResult<Vec<Relationship>> {
    let client = connect(&state).await?;
    let rows = client
        .query(
            "SELECT ns.nspname::text, src.relname::text, nt.nspname::text,
                dst.relname::text, c.conname::text, pg_catalog.pg_get_constraintdef(c.oid)::text
         FROM pg_catalog.pg_constraint c
         JOIN pg_catalog.pg_class src ON src.oid=c.conrelid
         JOIN pg_catalog.pg_namespace ns ON ns.oid=src.relnamespace
         JOIN pg_catalog.pg_class dst ON dst.oid=c.confrelid
         JOIN pg_catalog.pg_namespace nt ON nt.oid=dst.relnamespace
         WHERE c.contype='f'
           AND has_schema_privilege(ns.oid,'USAGE')
           AND has_schema_privilege(nt.oid,'USAGE')
           AND has_table_privilege(src.oid,'SELECT')
           AND has_table_privilege(dst.oid,'SELECT')
         ORDER BY ns.nspname, src.relname, c.conname LIMIT 80",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    Ok(Json(
        rows.iter()
            .map(|r| Relationship {
                from_schema: r.get(0),
                from_table: r.get(1),
                to_schema: r.get(2),
                to_table: r.get(3),
                constraint: r.get(4),
                definition: r.get(5),
            })
            .collect(),
    ))
}

#[derive(Serialize)]
struct DbObject {
    schema: String,
    name: String,
    kind: String,
}

async fn schema(State(state): State<AppState>) -> ApiResult<Vec<DbObject>> {
    let client = connect(&state).await?;
    let rows = client
        .query(
            "SELECT n.nspname, c.relname, CASE WHEN c.relkind IN ('v','m') THEN 'view' ELSE 'table' END
             FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r','p','v','m','f')
               AND n.nspname NOT IN ('pg_catalog','information_schema')
               AND n.nspname NOT LIKE 'pg_toast%'
               AND has_schema_privilege(n.oid, 'USAGE')
               AND has_table_privilege(c.oid, 'SELECT')
             ORDER BY n.nspname, c.relname",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    Ok(Json(
        rows.iter()
            .map(|r| DbObject {
                schema: r.get(0),
                name: r.get(1),
                kind: r.get(2),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct ColumnRequest {
    schema: String,
    object: String,
}

#[derive(Serialize)]
struct ColumnInfo {
    name: String,
    data_type: String,
    nullable: bool,
    primary_key: bool,
}

async fn columns(
    State(state): State<AppState>,
    Query(req): Query<ColumnRequest>,
) -> ApiResult<Vec<ColumnInfo>> {
    let client = connect(&state).await?;
    let rows = client
        .query(
            "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                    EXISTS (
                      SELECT 1 FROM pg_catalog.pg_index i
                      WHERE i.indrelid = c.oid AND i.indisprimary
                        AND a.attnum = ANY(i.indkey)
                    )
             FROM pg_catalog.pg_attribute a
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = $2
               AND c.relkind IN ('r','p','v','m','f')
               AND a.attnum > 0 AND NOT a.attisdropped
               AND has_schema_privilege(n.oid, 'USAGE')
               AND has_table_privilege(c.oid, 'SELECT')
             ORDER BY a.attnum",
            &[&req.schema, &req.object],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    Ok(Json(
        rows.iter()
            .map(|r| ColumnInfo {
                name: r.get(0),
                data_type: r.get(1),
                nullable: r.get(2),
                primary_key: r.get(3),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct QueryRequest {
    sql: String,
}

#[derive(Serialize)]
struct QueryResponse {
    columns: Vec<String>,
    column_types: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    affected: u64,
    truncated: bool,
    duration_ms: u128,
}

async fn execute(
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> ApiResult<QueryResponse> {
    if req.sql.trim().is_empty() || req.sql.len() > 256_000 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "SQL is empty or exceeds 256 KB",
        ));
    }
    let client = connect(&state).await?;
    // PostgreSQL's extended-query parser rejects multiple statements.
    // This also avoids incorrectly splitting SQL on semicolons inside strings.
    let statement = client
        .prepare(&req.sql)
        .await
        .map_err(|e| postgres_error(&e))?;
    let started = Instant::now();
    let stream = client
        .simple_query_raw(&req.sql)
        .await
        .map_err(|e| postgres_error(&e))?;
    tokio::pin!(stream);
    let mut result = QueryResponse {
        columns: statement
            .columns()
            .iter()
            .map(|c| c.name().to_owned())
            .collect(),
        column_types: statement
            .columns()
            .iter()
            .map(|c| c.type_().name().to_owned())
            .collect(),
        rows: vec![],
        affected: 0,
        truncated: false,
        duration_ms: 0,
    };
    while let Some(message) = stream.try_next().await.map_err(|e| postgres_error(&e))? {
        match message {
            SimpleQueryMessage::Row(row) => {
                if result.rows.len() < state.max_rows {
                    result.rows.push(
                        (0..row.len())
                            .map(|i| row.get(i).map(str::to_owned))
                            .collect(),
                    );
                } else {
                    // Stop reading after the first excess row. Draining the
                    // stream would defeat the response cap for large results.
                    // Each request owns its connection, which is not reused
                    // after this incomplete result stream is dropped.
                    result.truncated = true;
                    break;
                }
            }
            SimpleQueryMessage::CommandComplete(count) => result.affected = count,
            _ => {}
        }
    }
    result.duration_ms = started.elapsed().as_millis();
    Ok(Json(result))
}

#[derive(Deserialize)]
struct PageRequest {
    schema: String,
    table: String,
    offset: Option<i64>,
    limit: Option<i64>,
    filter_column: Option<String>,
    filter: Option<String>,
    filter2_column: Option<String>,
    filter2: Option<String>,
    sort_column: Option<String>,
    sort_dir: Option<String>,
}

#[derive(Serialize)]
struct PageResponse {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    offset: i64,
    limit: i64,
    has_next: bool,
    stable_order: bool,
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// Share one catalog-validated query builder between JSON paging and CSV export.
// Only identifiers returned by pg_catalog are interpolated into SQL.
struct PreparedBrowse {
    columns: Vec<String>,
    select_sql: String,
    filter_patterns: Vec<String>,
    stable_order: bool,
}

async fn prepare_browse(
    client: &tokio_postgres::Client,
    req: &PageRequest,
) -> Result<PreparedBrowse, (StatusCode, Json<ApiError>)> {
    let relation = client
        .query_opt(
            "SELECT c.oid, n.nspname, c.relname
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2
           AND c.relkind IN ('r','p','v','m','f')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'SELECT')",
            &[&req.schema, &req.table],
        )
        .await
        .map_err(|e| postgres_error(&e))?
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Table not found or access denied"))?;
    let oid: u32 = relation.get(0);
    let schema: String = relation.get(1);
    let table: String = relation.get(2);
    let keys = client
        .query(
            "SELECT a.attname FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_attribute a ON a.attrelid=i.indrelid
            AND a.attnum = ANY(i.indkey)
         WHERE i.indrelid=$1 AND i.indisprimary
         ORDER BY array_position(i.indkey::smallint[],a.attnum::smallint)",
            &[&oid],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let column_rows = client
        .query(
            "SELECT attname::text, atttypid::oid FROM pg_catalog.pg_attribute
         WHERE attrelid = $1 AND attnum > 0 AND NOT attisdropped
         ORDER BY attnum",
            &[&oid],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let columns: Vec<String> = column_rows.iter().map(|r| r.get(0)).collect();
    if columns.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Relation has no visible columns",
        ));
    }
    let valid_column = |name: &str| columns.iter().any(|col| col == name);
    let mut predicates = Vec::new();
    let mut filter_patterns = Vec::new();
    // At most two AND-connected literal substring filters. Every column name
    // originates in pg_catalog, while all user-controlled text is bound.
    for (column, value) in [
        (req.filter_column.as_deref(), req.filter.as_deref()),
        (req.filter2_column.as_deref(), req.filter2.as_deref()),
    ] {
        if column.is_some_and(|name| !valid_column(name))
            || value.is_some_and(|text| text.chars().count() > 200)
            || (value.is_some_and(|text| !text.is_empty()) && column.is_none())
        {
            return Err(api_error(StatusCode::BAD_REQUEST, "Invalid table filter"));
        }
        if let (Some(name), Some(text)) = (column, value.filter(|text| !text.is_empty())) {
            // A literal case-insensitive substring: %, _ and # are not wildcards.
            let escaped = text.replace('#', "##").replace('%', "#%").replace('_', "#_");
            filter_patterns.push(format!("%{escaped}%"));
            predicates.push(format!(
                "source.{}::text ILIKE ${} ESCAPE '#'",
                quote_ident(name),
                filter_patterns.len()
            ));
        }
    }
    if req.sort_column.as_deref().is_some_and(|name| !valid_column(name)) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Invalid sort column"));
    }
    let direction = req.sort_dir.as_deref().unwrap_or("asc");
    if !matches!(direction, "asc" | "desc") || (req.sort_column.is_none() && req.sort_dir.is_some())
    {
        return Err(api_error(StatusCode::BAD_REQUEST, "Invalid sort direction"));
    }
    let predicate = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    let stable_order = !keys.is_empty();
    let mut ordering = Vec::new();
    if let Some(name) = &req.sort_column {
        ordering.push(format!(
            "source.{} {} NULLS LAST",
            quote_ident(name),
            if direction == "desc" { "DESC" } else { "ASC" }
        ));
    }
    for row in &keys {
        let name: String = row.get(0);
        if req.sort_column.as_ref() != Some(&name) {
            ordering.push(format!("source.{} ASC NULLS LAST", quote_ident(&name)));
        }
    }
    let order = if ordering.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", ordering.join(", "))
    };
    // Match the existing JSON paging representation, including t/f for bool.
    let projection = column_rows
        .iter()
        .map(|row| {
            let name: String = row.get(0);
            let typid: u32 = row.get(1);
            let expr = if typid == 16 {
                format!("pg_catalog.boolout({})::text", quote_ident(&name))
            } else {
                format!("{}::text", quote_ident(&name))
            };
            format!("{expr} AS {}", quote_ident(&name))
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(PreparedBrowse {
        columns,
        select_sql: format!(
            "SELECT {projection} FROM {}.{} AS source{predicate}{order}",
            quote_ident(&schema),
            quote_ident(&table)
        ),
        filter_patterns,
        stable_order,
    })
}

async fn table_rows(
    State(state): State<AppState>,
    Query(req): Query<PageRequest>,
) -> ApiResult<PageResponse> {
    let offset = req.offset.unwrap_or(0);
    let limit = req.limit.unwrap_or(100);
    if !(0..=1_000_000).contains(&offset) || !(1..=200).contains(&limit) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Invalid offset or page size",
        ));
    }
    let client = connect(&state).await?;
    let prepared = prepare_browse(&client, &req).await?;
    let sql = format!(
        "{} LIMIT {} OFFSET {}",
        prepared.select_sql,
        limit + 1,
        offset
    );
    let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = prepared
        .filter_patterns
        .iter()
        .map(|pattern| pattern as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    let db_rows = client.query(&sql, &params).await.map_err(|e| postgres_error(&e))?;
    let mut rows: Vec<Vec<Option<String>>> = db_rows
        .iter()
        .map(|row| {
            (0..prepared.columns.len())
                .map(|i| row.get::<usize, Option<String>>(i))
                .collect()
        })
        .collect();
    let has_next = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(Json(PageResponse {
        columns: prepared.columns,
        rows,
        offset,
        limit,
        has_next,
        stable_order: prepared.stable_order,
    }))
}

const EXPORT_MAX_ROWS: usize = 10_000;
const EXPORT_MAX_BYTES: usize = 10 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRequest {
    schema: String,
    table: String,
    filter_column: Option<String>,
    filter: Option<String>,
    filter2_column: Option<String>,
    filter2: Option<String>,
    sort_column: Option<String>,
    sort_dir: Option<String>,
}

// Excel/LibreOffice may interpret whitespace-prefixed =,+,-,@ as formulas.
fn csv_formula_prefix(value: &str) -> bool {
    value
        .chars()
        .find(|ch| !ch.is_whitespace() && *ch != '\u{feff}' && *ch != '\u{200b}')
        .is_some_and(|ch| matches!(ch, '=' | '+' | '-' | '@'))
}

// Bounded serialization: check encoded UTF-8 size *before* appending a field.
fn csv_append_field(out: &mut String, value: &str, cap: usize) -> bool {
    let guard = csv_formula_prefix(value);
    let quote_count = value.bytes().filter(|ch| *ch == b'"').count();
    let needed = value
        .len()
        .saturating_add(quote_count)
        .saturating_add(2 + usize::from(guard));
    if needed > cap.saturating_sub(out.len()) {
        return false;
    }
    out.push('"');
    if guard {
        out.push('\'');
    }
    for ch in value.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    true
}

fn csv_append_record<'a>(
    out: &mut String,
    values: impl Iterator<Item = Option<&'a str>>,
    cap: usize,
) -> bool {
    for (i, value) in values.enumerate() {
        if i > 0 {
            if out.len() >= cap {
                return false;
            }
            out.push(',');
        }
        // PostgreSQL CSV convention: NULL is unquoted empty, "" is empty text.
        if let Some(value) = value {
            if !csv_append_field(out, value, cap) {
                return false;
            }
        }
    }
    if cap.saturating_sub(out.len()) < 2 {
        return false;
    }
    out.push_str("\r\n");
    true
}

// A single database statement uses one consistent snapshot. The complete body
// is capped before HTTP headers are sent, so quota failures cannot silently
// produce a truncated "successful" CSV. This deliberately buffers <=10 MiB:
// an unbounded HTTP stream cannot change to 413 after sending its headers.
async fn table_export(
    State(state): State<AppState>,
    Query(req): Query<ExportRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let client = connect(&state).await?;
    // connect() already began a read-only transaction for restricted profiles.
    // Change its isolation before executing any SELECT.
    let begin = if state.read_only {
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"
    } else {
        "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY"
    };
    client
        .batch_execute(begin)
        .await
        .map_err(|e| postgres_error(&e))?;
    let page = PageRequest {
        schema: req.schema,
        table: req.table,
        offset: None,
        limit: None,
        filter_column: req.filter_column,
        filter: req.filter,
        filter2_column: req.filter2_column,
        filter2: req.filter2,
        sort_column: req.sort_column,
        sort_dir: req.sort_dir,
    };
    let prepared = prepare_browse(&client, &page).await?;
    let sql = format!("{} LIMIT {}", prepared.select_sql, EXPORT_MAX_ROWS + 1);
    let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = prepared
        .filter_patterns
        .iter()
        .map(|pattern| pattern as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    let rows = client
        .query_raw(&sql, params)
        .await
        .map_err(|e| postgres_error(&e))?;
    tokio::pin!(rows);

    let mut csv = String::with_capacity(64 * 1024);
    csv.push('\u{feff}'); // UTF-8 BOM for spreadsheet compatibility
    if !csv_append_record(
        &mut csv,
        prepared.columns.iter().map(|s| Some(s.as_str())),
        EXPORT_MAX_BYTES,
    ) {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "CSV export exceeds 10 MiB",
        ));
    }
    let mut count = 0;
    while let Some(row) = rows.try_next().await.map_err(|e| postgres_error(&e))? {
        if count == EXPORT_MAX_ROWS {
            return Err(api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "CSV export exceeds 10,000 rows; narrow the filter",
            ));
        }
        let values: Vec<Option<String>> = (0..prepared.columns.len())
            .map(|i| row.get::<usize, Option<String>>(i))
            .collect();
        if !csv_append_record(
            &mut csv,
            values.iter().map(|v| v.as_deref()),
            EXPORT_MAX_BYTES,
        ) {
            return Err(api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "CSV export exceeds 10 MiB; narrow the filter",
            ));
        }
        count += 1;
    }
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|e| postgres_error(&e))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"datawizz-filtered-table.csv\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
        ],
        csv,
    ))
}

#[derive(Deserialize)]
struct RowChange {
    schema: String,
    table: String,
    original: Vec<Option<String>>,
    values: Option<Vec<Option<String>>>,
    delete: bool,
}

#[derive(Serialize)]
struct ChangeResult {
    affected: u64,
}

async fn change_row(
    State(state): State<AppState>,
    Json(req): Json<RowChange>,
) -> ApiResult<ChangeResult> {
    if state.read_only {
        return Err(api_error(StatusCode::FORBIDDEN, "Read-only profile"));
    }
    let mut client = connect(&state).await?;
    // Only base/partitioned tables, never views, foreign tables or relations without a PK.
    let relation = client
        .query_opt(
            "SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind IN ('r','p')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'SELECT')
           AND has_table_privilege(c.oid,CASE WHEN $3 THEN 'DELETE' ELSE 'UPDATE' END)",
            &[&req.schema, &req.table, &req.delete],
        )
        .await
        .map_err(|e| postgres_error(&e))?
        .ok_or_else(|| {
            api_error(
                StatusCode::FORBIDDEN,
                "Table is not editable or access denied",
            )
        })?;
    let oid: u32 = relation.get(0);
    let schema: String = relation.get(1);
    let table: String = relation.get(2);
    let columns = client
        .query(
            "SELECT a.attname, pg_catalog.format_type(a.atttypid,a.atttypmod),
                EXISTS (SELECT 1 FROM pg_catalog.pg_index i
                        WHERE i.indrelid=$1 AND i.indisprimary
                        AND a.attnum = ANY(i.indkey)),
                a.attgenerated <> '' OR a.attidentity <> ''
         FROM pg_catalog.pg_attribute a
         WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped
         ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    if columns.is_empty() || !columns.iter().any(|c| c.get::<_, bool>(2)) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Primary key required"));
    }
    if req.original.len() != columns.len()
        || (!req.delete && req.values.as_ref().map(Vec::len) != Some(columns.len()))
    {
        return Err(api_error(StatusCode::BAD_REQUEST, "Row columns mismatch"));
    }
    let mut args: Vec<Option<String>> = Vec::new();
    let mut where_parts = Vec::new();
    for (i, c) in columns.iter().enumerate() {
        // Compare the full originally loaded row to reject concurrent modifications.
        let name: String = c.get(0);
        args.push(req.original[i].clone());
        where_parts.push(format!(
            "{} IS NOT DISTINCT FROM CAST(${}::text AS {})",
            quote_ident(&name),
            args.len(),
            c.get::<_, String>(1)
        ));
    }
    let qualified = format!("{}.{}", quote_ident(&schema), quote_ident(&table));
    let sql = if req.delete {
        format!(
            "DELETE FROM {qualified} WHERE {} RETURNING 1",
            where_parts.join(" AND ")
        )
    } else {
        let mut assignments = Vec::new();
        let values = req.values.as_ref().expect("checked above");
        for (i, c) in columns.iter().enumerate() {
            if values[i] == req.original[i] {
                continue;
            }
            if c.get::<_, bool>(2) || c.get::<_, bool>(3) {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "Primary key and generated columns cannot be edited",
                ));
            }
            let name: String = c.get(0);
            let datatype: String = c.get(1);
            args.push(values[i].clone());
            assignments.push(format!(
                "{} = CAST(${}::text AS {})",
                quote_ident(&name),
                args.len(),
                datatype
            ));
        }
        if assignments.is_empty() {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "No changed editable fields",
            ));
        }
        format!(
            "UPDATE {qualified} SET {} WHERE {} RETURNING 1",
            assignments.join(", "),
            where_parts.join(" AND ")
        )
    };
    // Execute atomically; RETURNING verifies one row before committing.
    let transaction = client.transaction().await.map_err(|e| postgres_error(&e))?;
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = args
        .iter()
        .map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    let rows = transaction
        .query(&sql, &refs)
        .await
        .map_err(|e| postgres_error(&e))?;
    if rows.len() != 1 {
        return Err(api_error(
            StatusCode::CONFLICT,
            "Row changed or no longer exists; refresh and retry",
        ));
    }
    transaction.commit().await.map_err(|e| postgres_error(&e))?;
    Ok(Json(ChangeResult { affected: 1 }))
}

#[derive(Deserialize)]
struct ChangesetRequest {
    schema: String,
    table: String,
    changes: Vec<ChangesetItem>,
}

#[derive(Deserialize)]
struct ChangesetItem {
    original: Vec<Option<String>>,
    values: Vec<Option<String>>,
}

// All rows are updated on ONE PostgreSQL connection and ONE transaction.
// Any error or stale original row drops the transaction without a commit.
async fn apply_changeset(
    State(state): State<AppState>,
    Json(req): Json<ChangesetRequest>,
) -> ApiResult<ChangeResult> {
    if state.read_only {
        return Err(api_error(StatusCode::FORBIDDEN, "Read-only profile"));
    }
    if req.changes.is_empty() || req.changes.len() > 20 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Changeset must contain 1 to 20 rows",
        ));
    }
    let mut client = connect(&state).await?;
    let relation = client
        .query_opt(
            "SELECT c.oid,n.nspname,c.relname FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind IN ('r','p')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'SELECT')
           AND has_table_privilege(c.oid,'UPDATE')",
            &[&req.schema, &req.table],
        )
        .await
        .map_err(|e| postgres_error(&e))?
        .ok_or_else(|| {
            api_error(
                StatusCode::FORBIDDEN,
                "Table is not editable or access denied",
            )
        })?;
    let oid: u32 = relation.get(0);
    let schema: String = relation.get(1);
    let table: String = relation.get(2);
    let columns = client
        .query(
            "SELECT a.attname,pg_catalog.format_type(a.atttypid,a.atttypmod),
                EXISTS (SELECT 1 FROM pg_catalog.pg_index i
                        WHERE i.indrelid=$1 AND i.indisprimary
                          AND a.attnum=ANY(i.indkey)),
                a.attgenerated <> '' OR a.attidentity <> ''
         FROM pg_catalog.pg_attribute a
         WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped
         ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    if columns.is_empty() || !columns.iter().any(|c| c.get::<_, bool>(2)) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Primary key required"));
    }
    let qualified = format!("{}.{}", quote_ident(&schema), quote_ident(&table));
    let transaction = client.transaction().await.map_err(|e| postgres_error(&e))?;
    for change in &req.changes {
        if change.original.len() != columns.len() || change.values.len() != columns.len() {
            return Err(api_error(StatusCode::BAD_REQUEST, "Row columns mismatch"));
        }
        let mut args: Vec<Option<String>> = Vec::new();
        let mut predicates = Vec::new();
        let mut assignments = Vec::new();
        for (i, col) in columns.iter().enumerate() {
            let name: String = col.get(0);
            args.push(change.original[i].clone());
            predicates.push(format!(
                "{} IS NOT DISTINCT FROM CAST(${}::text AS {})",
                quote_ident(&name),
                args.len(),
                col.get::<_, String>(1)
            ));
        }
        for (i, col) in columns.iter().enumerate() {
            if change.values[i] == change.original[i] {
                continue;
            }
            if col.get::<_, bool>(2) || col.get::<_, bool>(3) {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "Primary key and generated columns cannot be edited",
                ));
            }
            let name: String = col.get(0);
            let datatype: String = col.get(1);
            args.push(change.values[i].clone());
            assignments.push(format!(
                "{}=CAST(${}::text AS {})",
                quote_ident(&name),
                args.len(),
                datatype
            ));
        }
        if assignments.is_empty() {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "No changed editable fields",
            ));
        }
        let sql = format!(
            "UPDATE {qualified} SET {} WHERE {} RETURNING 1",
            assignments.join(", "),
            predicates.join(" AND ")
        );
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = args
            .iter()
            .map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        let rows = transaction
            .query(&sql, &refs)
            .await
            .map_err(|e| postgres_error(&e))?;
        if rows.len() != 1 {
            return Err(api_error(
                StatusCode::CONFLICT,
                "Changeset conflict; no rows committed",
            ));
        }
    }
    transaction.commit().await.map_err(|e| postgres_error(&e))?;
    Ok(Json(ChangeResult {
        affected: req.changes.len() as u64,
    }))
}

#[derive(Deserialize)]
struct InsertField {
    column: String,
    // null means SQL NULL, while omitting a column uses its database DEFAULT.
    value: Option<String>,
}
#[derive(Deserialize)]
struct InsertRow {
    schema: String,
    table: String,
    fields: Vec<InsertField>,
}

async fn insert_row(
    State(state): State<AppState>,
    Json(req): Json<InsertRow>,
) -> ApiResult<ChangeResult> {
    if state.read_only {
        return Err(api_error(StatusCode::FORBIDDEN, "Read-only profile"));
    }
    let client = connect(&state).await?;
    let relation = client
        .query_opt(
            "SELECT c.oid,n.nspname,c.relname FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind IN ('r','p')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'INSERT')",
            &[&req.schema, &req.table],
        )
        .await
        .map_err(|e| postgres_error(&e))?
        .ok_or_else(|| {
            api_error(
                StatusCode::FORBIDDEN,
                "Table is not insertable or access denied",
            )
        })?;
    let oid: u32 = relation.get(0);
    let schema: String = relation.get(1);
    let table: String = relation.get(2);
    let metadata = client
        .query(
            "SELECT a.attname,pg_catalog.format_type(a.atttypid,a.atttypmod),
                a.attgenerated <> '' OR a.attidentity <> ''
         FROM pg_catalog.pg_attribute a
         WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped
         ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    if req.fields.len() > metadata.len() {
        return Err(api_error(StatusCode::BAD_REQUEST, "Too many insert fields"));
    }
    let mut names = Vec::new();
    let mut expressions = Vec::new();
    let mut values: Vec<Option<String>> = Vec::new();
    for field in &req.fields {
        let column = metadata
            .iter()
            .find(|r| r.get::<_, String>(0) == field.column)
            .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "Unknown insert column"))?;
        if column.get::<_, bool>(2) || names.iter().any(|n: &String| n == &field.column) {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "Duplicate or generated insert column",
            ));
        }
        names.push(field.column.clone());
        values.push(field.value.clone());
        let datatype: String = column.get(1);
        expressions.push(format!("CAST(${}::text AS {})", values.len(), datatype));
    }
    let qualified = format!("{}.{}", quote_ident(&schema), quote_ident(&table));
    let sql = if names.is_empty() {
        format!("INSERT INTO {qualified} DEFAULT VALUES")
    } else {
        format!(
            "INSERT INTO {qualified} ({}) VALUES ({})",
            names
                .iter()
                .map(|n| quote_ident(n))
                .collect::<Vec<_>>()
                .join(", "),
            expressions.join(", ")
        )
    };
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = values
        .iter()
        .map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    let affected = client
        .execute(&sql, &refs)
        .await
        .map_err(|e| postgres_error(&e))?;
    if affected != 1 {
        return Err(api_error(
            StatusCode::CONFLICT,
            "Insert did not affect exactly one row",
        ));
    }
    Ok(Json(ChangeResult { affected }))
}

// Read-only PostgreSQL administration inventory. No password hashes, session SQL text or client addresses.
#[derive(Serialize)]
struct AdminDatabase {
    name: String,
    owner: String,
    allow_connections: bool,
}

#[derive(Serialize)]
struct AdminRole {
    name: String,
    can_login: bool,
    superuser: bool,
    create_db: bool,
    create_role: bool,
}

#[derive(Serialize)]
struct AdminMembership {
    role: String,
    member: String,
    admin_option: bool,
}

#[derive(Serialize)]
struct AdminSession {
    pid: i32,
    database: Option<String>,
    username: Option<String>,
    state: Option<String>,
    wait_event_type: Option<String>,
}

#[derive(Serialize)]
struct AdminExtension {
    name: String,
    version: String,
    schema: String,
}

#[derive(Serialize)]
struct AdminSchema {
    name: String,
    owner: String,
    can_use: bool,
    can_create: bool,
}

#[derive(Serialize)]
struct AdminRelation {
    schema: String,
    name: String,
    kind: String,
    owner: String,
    estimated_rows: i64,
    size_bytes: i64,
    // View source is available only through PostgreSQL's own catalog visibility.
    definition: Option<String>,
}

#[derive(Serialize)]
struct AdminIndex {
    schema: String,
    table: String,
    name: String,
    valid: bool,
    scans: i64,
    definition: String,
}

#[derive(Serialize)]
struct AdminConstraint {
    schema: String,
    table: String,
    name: String,
    kind: String,
    definition: String,
}

#[derive(Serialize)]
struct AdminTableStats {
    schema: String,
    table: String,
    live_rows: i64,
    dead_rows: i64,
    seq_scans: i64,
    index_scans: i64,
    last_autovacuum: Option<String>,
    last_autoanalyze: Option<String>,
}

#[derive(Serialize)]
struct AdminLock {
    pid: Option<i32>,
    relation: String,
    mode: String,
    granted: bool,
}

#[derive(Serialize)]
struct AdminSetting {
    name: String,
    value: String,
    unit: Option<String>,
    pending_restart: bool,
}

#[derive(Serialize)]
struct AdminPrivilege {
    schema: String,
    table: String,
    grantee: String,
    privilege: String,
    grantable: bool,
}

#[derive(Serialize)]
struct AdminRoutine {
    schema: String,
    name: String,
    arguments: String,
    kind: String,
    owner: String,
}

#[derive(Serialize)]
struct AdminOverview {
    databases: Vec<AdminDatabase>,
    roles: Vec<AdminRole>,
    memberships: Vec<AdminMembership>,
    sessions: Vec<AdminSession>,
    extensions: Vec<AdminExtension>,
    schemas: Vec<AdminSchema>,
    relations: Vec<AdminRelation>,
    indexes: Vec<AdminIndex>,
    constraints: Vec<AdminConstraint>,
    table_stats: Vec<AdminTableStats>,
    locks: Vec<AdminLock>,
    settings: Vec<AdminSetting>,
    privileges: Vec<AdminPrivilege>,
    routines: Vec<AdminRoutine>,
}

async fn admin_overview(State(state): State<AppState>) -> ApiResult<AdminOverview> {
    let client = connect(&state).await?;
    // Each catalog query is fixed, read-only and bounded. PostgreSQL controls which metadata is visible.
    let databases = client
        .query(
            "SELECT datname::text, pg_catalog.pg_get_userbyid(datdba)::text, datallowconn
         FROM pg_catalog.pg_database ORDER BY datname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let roles = client
        .query(
            "SELECT rolname::text, rolcanlogin, rolsuper, rolcreatedb, rolcreaterole
         FROM pg_catalog.pg_roles ORDER BY rolname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let memberships = client
        .query(
            "SELECT r.rolname::text, m.rolname::text, am.admin_option
         FROM pg_catalog.pg_auth_members am
         JOIN pg_catalog.pg_roles r ON r.oid = am.roleid
         JOIN pg_catalog.pg_roles m ON m.oid = am.member
         ORDER BY r.rolname, m.rolname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let sessions = client
        .query(
            "SELECT pid, datname::text, usename::text, state::text, wait_event_type::text
         FROM pg_catalog.pg_stat_activity ORDER BY pid LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let extensions = client
        .query(
            "SELECT e.extname::text, e.extversion::text, n.nspname::text
         FROM pg_catalog.pg_extension e
         JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
         ORDER BY e.extname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;

    let schemas = client
        .query(
            "SELECT n.nspname::text, pg_catalog.pg_get_userbyid(n.nspowner)::text,
                has_schema_privilege(n.oid, 'USAGE'), has_schema_privilege(n.oid, 'CREATE')
         FROM pg_catalog.pg_namespace n
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let relations = client.query(
        "SELECT n.nspname::text, c.relname::text,
                CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned table'
                     WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view'
                     WHEN 'f' THEN 'foreign table' WHEN 'S' THEN 'sequence' END::text,
                pg_catalog.pg_get_userbyid(c.relowner)::text,
                GREATEST(c.reltuples::bigint, 0),
                CASE WHEN c.relkind IN ('r','p','m','S') THEN pg_catalog.pg_total_relation_size(c.oid)
                     ELSE 0::bigint END,
                CASE WHEN c.relkind IN ('v','m') THEN pg_catalog.pg_get_viewdef(c.oid, true)::text
                     ELSE NULL::text END
         FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('r','p','v','m','f','S')
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname, c.relname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let indexes = client
        .query(
            "SELECT s.schemaname::text, s.relname::text, s.indexrelname::text,
                i.indisvalid, COALESCE(s.idx_scan, 0)::bigint,
                pg_catalog.pg_get_indexdef(s.indexrelid)::text
         FROM pg_catalog.pg_stat_user_indexes s
         JOIN pg_catalog.pg_index i ON i.indexrelid = s.indexrelid
         ORDER BY s.schemaname, s.relname, s.indexrelname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let constraints = client
        .query(
            "SELECT n.nspname::text, c.relname::text, con.conname::text,
                con.contype::text, pg_catalog.pg_get_constraintdef(con.oid)::text
         FROM pg_catalog.pg_constraint con
         JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname, c.relname, con.conname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let table_stats = client
        .query(
            "SELECT s.schemaname::text, s.relname::text, s.n_live_tup::bigint,
                s.n_dead_tup::bigint, s.seq_scan::bigint, COALESCE(s.idx_scan, 0)::bigint,
                to_char(s.last_autovacuum, 'YYYY-MM-DD HH24:MI')::text,
                to_char(s.last_autoanalyze, 'YYYY-MM-DD HH24:MI')::text
         FROM pg_catalog.pg_stat_user_tables s
         ORDER BY s.schemaname, s.relname LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let locks = client
        .query(
            "SELECT l.pid,
                (n.nspname || '.' || c.relname)::text,
                l.mode::text, l.granted
         FROM pg_catalog.pg_locks l
         JOIN pg_catalog.pg_class c ON c.oid = l.relation
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE l.locktype = 'relation'
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY l.granted, l.pid NULLS LAST LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    // Explicit allowlist: pg_settings may contain infrastructure paths and sensitive values.
    let settings = client
        .query(
            "SELECT name::text, setting::text, unit::text, pending_restart
         FROM pg_catalog.pg_settings
         WHERE name IN ('server_version', 'max_connections', 'shared_buffers', 'work_mem',
                        'maintenance_work_mem', 'statement_timeout', 'autovacuum',
                        'max_wal_size', 'wal_level', 'idle_in_transaction_session_timeout')
         ORDER BY name LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let privileges = client
        .query(
            "SELECT table_schema::text, table_name::text, grantee::text,
                privilege_type::text, (is_grantable = 'YES')
         FROM information_schema.role_table_grants
         ORDER BY table_schema, table_name, grantee, privilege_type LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;
    let routines = client
        .query(
            "SELECT n.nspname::text, p.proname::text,
                pg_catalog.pg_get_function_identity_arguments(p.oid)::text,
                CASE WHEN p.prokind = 'p' THEN 'procedure' ELSE 'function' END::text,
                pg_catalog.pg_get_userbyid(p.proowner)::text
         FROM pg_catalog.pg_proc p
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname, p.proname, p.oid LIMIT 100",
            &[],
        )
        .await
        .map_err(|e| postgres_error(&e))?;

    Ok(Json(AdminOverview {
        databases: databases
            .iter()
            .map(|r| AdminDatabase {
                name: r.get(0),
                owner: r.get(1),
                allow_connections: r.get(2),
            })
            .collect(),
        roles: roles
            .iter()
            .map(|r| AdminRole {
                name: r.get(0),
                can_login: r.get(1),
                superuser: r.get(2),
                create_db: r.get(3),
                create_role: r.get(4),
            })
            .collect(),
        memberships: memberships
            .iter()
            .map(|r| AdminMembership {
                role: r.get(0),
                member: r.get(1),
                admin_option: r.get(2),
            })
            .collect(),
        sessions: sessions
            .iter()
            .map(|r| AdminSession {
                pid: r.get(0),
                database: r.get(1),
                username: r.get(2),
                state: r.get(3),
                wait_event_type: r.get(4),
            })
            .collect(),
        extensions: extensions
            .iter()
            .map(|r| AdminExtension {
                name: r.get(0),
                version: r.get(1),
                schema: r.get(2),
            })
            .collect(),

        schemas: schemas
            .iter()
            .map(|r| AdminSchema {
                name: r.get(0),
                owner: r.get(1),
                can_use: r.get(2),
                can_create: r.get(3),
            })
            .collect(),
        relations: relations
            .iter()
            .map(|r| AdminRelation {
                schema: r.get(0),
                name: r.get(1),
                kind: r.get(2),
                owner: r.get(3),
                estimated_rows: r.get(4),
                size_bytes: r.get(5),
                definition: r.get(6),
            })
            .collect(),
        indexes: indexes
            .iter()
            .map(|r| AdminIndex {
                schema: r.get(0),
                table: r.get(1),
                name: r.get(2),
                valid: r.get(3),
                scans: r.get(4),
                definition: r.get(5),
            })
            .collect(),
        constraints: constraints
            .iter()
            .map(|r| AdminConstraint {
                schema: r.get(0),
                table: r.get(1),
                name: r.get(2),
                kind: r.get(3),
                definition: r.get(4),
            })
            .collect(),
        table_stats: table_stats
            .iter()
            .map(|r| AdminTableStats {
                schema: r.get(0),
                table: r.get(1),
                live_rows: r.get(2),
                dead_rows: r.get(3),
                seq_scans: r.get(4),
                index_scans: r.get(5),
                last_autovacuum: r.get(6),
                last_autoanalyze: r.get(7),
            })
            .collect(),
        locks: locks
            .iter()
            .map(|r| AdminLock {
                pid: r.get(0),
                relation: r.get(1),
                mode: r.get(2),
                granted: r.get(3),
            })
            .collect(),
        settings: settings
            .iter()
            .map(|r| AdminSetting {
                name: r.get(0),
                value: r.get(1),
                unit: r.get(2),
                pending_restart: r.get(3),
            })
            .collect(),
        privileges: privileges
            .iter()
            .map(|r| AdminPrivilege {
                schema: r.get(0),
                table: r.get(1),
                grantee: r.get(2),
                privilege: r.get(3),
                grantable: r.get(4),
            })
            .collect(),
        routines: routines
            .iter()
            .map(|r| AdminRoutine {
                schema: r.get(0),
                name: r.get(1),
                arguments: r.get(2),
                kind: r.get(3),
                owner: r.get(4),
            })
            .collect(),
    }))
}

async fn index() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("../web/index.html")),
    )
}

async fn sql_editor_script() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../web/vendor/sql-editor.js"),
    )
}

async fn connection_manager_script() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../web/connection-manager.js"),
    )
}

fn api_routes(state: AppState) -> Router {
    Router::new()
        .route("/api/connections", get(connections))
        .route("/api/connection-status", get(connection_status))
        .route("/api/relationships", get(relationships))
        .route("/api/schema", get(schema))
        .route("/api/admin", get(admin_overview))
        .route("/api/columns", get(columns))
        .route("/api/query", post(execute))
        .route("/api/table-rows", get(table_rows))
        .route("/api/table-export", get(table_export))
        .route("/api/row-change", post(change_row))
        .route("/api/changeset", post(apply_changeset))
        .route("/api/row-insert", post(insert_row))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_access_token,
        ))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    let url = env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let max_rows = env::var("MAX_ROWS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    let timeout_ms = env::var("QUERY_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10_000)
        .clamp(100, 60_000);
    let tls_mode = env::var("DATABASE_TLS").unwrap_or_else(|_| "disable".into());
    let tls = verified_pg_tls(&tls_mode, env::var("DATABASE_TLS_CA_FILE").ok().as_deref())
        .expect("Invalid PostgreSQL TLS configuration");
    let access_token = env::var("DATAWIZZ_ACCESS_TOKEN").ok().map(|value| {
        assert!(
            value.len() >= 32 && value.len() <= 4096,
            "DATAWIZZ_ACCESS_TOKEN must contain 32..4096 characters"
        );
        Arc::new(value)
    });
    // Credentials remain on the server; profile routes bind one immutable AppState.
    let extras =
        configured_profiles(&env::var("DATABASE_PROFILES_JSON").unwrap_or_else(|_| "[]".into()))
            .expect("Invalid profile configuration");
    assert!(
        extras.is_empty() || access_token.is_some(),
        "DATAWIZZ_ACCESS_TOKEN is mandatory when additional profiles are configured"
    );
    let mut visible = vec![ConnectionOption {
        id: "default".into(),
        label: "Default PostgreSQL".into(),
        engine: "postgresql".into(),
        environment: "dev".into(),
        read_only: false,
    }];
    visible.extend(extras.iter().map(|p| ConnectionOption {
        id: p.id.clone(),
        label: p.label.clone(),
        engine: "postgresql".into(),
        environment: p.environment.clone(),
        read_only: p.read_only,
    }));
    let profiles = Arc::new(visible);
    let managed = env::var("DATAWIZZ_PROFILE_STORE")
        .ok()
        .filter(|path| !path.is_empty())
        .map(|path| {
            assert!(
                access_token.is_some(),
                "Managed connections require DATAWIZZ_ACCESS_TOKEN"
            );
            Arc::new(
                connection_manager::ManagedStore::load(&path, &profiles)
                    .expect("Invalid managed connection store"),
            )
        });
    let state = AppState {
        url: Arc::new(url),
        max_rows,
        timeout_ms,
        tls,
        access_token: access_token.clone(),
        read_only: false,
        profiles: profiles.clone(),
        managed: managed.clone(),
    };
    // Preserve the existing fail-fast check for the default PostgreSQL connection.
    connect(&state).await.expect("Cannot connect to PostgreSQL");
    let mut app = Router::new()
        .route("/", get(index))
        .route("/assets/sql-editor.js", get(sql_editor_script))
        .route(
            "/assets/connection-manager.js",
            get(connection_manager_script),
        )
        .merge(api_routes(state.clone()))
        .merge(connection_manager::routes(state.clone()));
    for profile in extras {
        let tls_mode = profile.tls.as_deref().unwrap_or("disable");
        let tls = verified_pg_tls(tls_mode, profile.ca_file.as_deref())
            .expect("Invalid TLS setting in DATABASE_PROFILES_JSON");
        let profile_state = AppState {
            url: Arc::new(profile.url),
            max_rows,
            timeout_ms,
            tls,
            access_token: access_token.clone(),
            read_only: profile.read_only,
            profiles: profiles.clone(),
            managed: managed.clone(),
        };
        // A static path prefix selects the server-side DB. No client DSN or
        // mutable global "current database" that could cross user requests.
        app = app.nest(
            &format!("/profiles/{}", profile.id),
            api_routes(profile_state),
        );
    }
    if managed.is_some() {
        app = app.route(
            "/profiles/{id}/{*tail}",
            axum::routing::any(connection_manager::dispatch).with_state(state.clone()),
        );
    }
    let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let address = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .expect("Cannot bind port");
    println!("Datawizz ready at http://{address}");
    axum::serve(listener, app)
        .await
        .expect("HTTP server failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_export_escapes_formulas_unicode_quotes_and_distinguishes_null() {
        let mut csv = String::new();
        assert!(csv_append_record(
            &mut csv,
            [
                Some("normal"),
                Some(" =1+1"),
                Some("-2"),
                Some("@SUM(A1)"),
                Some("Unicode: ü😀"),
                Some("quoted \"value\"\r\nnext"),
                Some(""),
                None,
            ]
            .into_iter(),
            10_000
        ));
        assert!(csv.contains("\"' =1+1\""));
        assert!(csv.contains("\"'-2\""));
        assert!(csv.contains("\"'@SUM(A1)\""));
        assert!(csv.contains("\"Unicode: ü😀\""));
        assert!(csv.contains("\"quoted \"\"value\"\"\r\nnext\""));
        assert!(csv.ends_with(&['"', '"', ',', '\r', '\n'].iter().collect::<String>()));
        assert!(!csv_formula_prefix("plain - text"));
        assert!(csv_formula_prefix("\u{feff}\u{200b} \t=SUM(A1)"));
        assert!(csv_formula_prefix("\u{200b}+SUM(A1)"));
    }

    #[test]
    fn csv_export_rejects_oversized_rows_without_appending_value() {
        let mut csv = String::new();
        assert!(!csv_append_record(
            &mut csv,
            [Some("abcdefgh")].into_iter(),
            5
        ));
        assert!(csv.len() <= 5);
        assert!(!csv_append_record(&mut csv, [Some("x\"x")].into_iter(), 5));
        assert!(csv.len() <= 5);
    }

    #[tokio::test]
    async fn snapshot_export_filters_quotes_enforces_quota_and_preserves_data() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_csv_export_test (
                id integer PRIMARY KEY, label text, nullable text
             );
             INSERT INTO public.datawizz_csv_export_test VALUES
                 (1, '=2+2', NULL), (2, 'percent%_# ü', ''), (3, 'quoted \"text\"', 'safe');",
            )
            .await
            .expect("export fixture");
        let export = ExportRequest {
            schema: "public".into(),
            table: "datawizz_csv_export_test".into(),
            filter_column: Some("label".into()),
            filter: Some("%_#".into()),
            filter2_column: None,
            filter2: None,
            sort_column: Some("id".into()),
            sort_dir: Some("desc".into()),
        };
        let body = table_export(State(state.clone()), Query(export))
            .await
            .expect("snapshot export")
            .into_response();
        assert_eq!(body.status(), StatusCode::OK);
        assert_eq!(
            body.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/csv; charset=utf-8"
        );
        assert_eq!(
            body.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let bytes = axum::body::to_bytes(body.into_body(), EXPORT_MAX_BYTES)
            .await
            .expect("bounded CSV");
        let output = std::str::from_utf8(&bytes).expect("UTF-8 CSV");
        assert!(output.starts_with("\u{feff}"));
        assert!(output.contains("\"percent%_# ü\""));
        assert!(!output.contains("\"'=2+2\""), "filter must restrict rows");

        let all = table_export(
            State(state.clone()),
            Query(ExportRequest {
                schema: "public".into(),
                table: "datawizz_csv_export_test".into(),
                filter_column: None,
                filter: None,
                filter2_column: None,
                filter2: None,
                sort_column: Some("id".into()),
                sort_dir: Some("asc".into()),
            }),
        )
        .await
        .expect("export all")
        .into_response();
        let bytes = axum::body::to_bytes(all.into_body(), EXPORT_MAX_BYTES)
            .await
            .expect("bounded CSV");
        let output = std::str::from_utf8(&bytes).unwrap();
        assert!(output.contains("'=2+2"));
        assert!(output.contains("quoted \"\"text\"\""));
        assert!(output.contains(",\r\n"), "NULL should be unquoted empty");
        assert!(
            output.contains(",\"\"\r\n"),
            "empty string should be quoted"
        );

        let bad = table_export(
            State(state.clone()),
            Query(ExportRequest {
                schema: "public".into(),
                table: "datawizz_csv_export_test".into(),
                filter_column: Some("id); DROP TABLE x; --".into()),
                filter: Some("1".into()),
                filter2_column: None,
                filter2: None,
                sort_column: None,
                sort_dir: None,
            }),
        )
        .await;
        assert!(matches!(bad, Err((StatusCode::BAD_REQUEST, _))));

        let exists: i64 = client
            .query_one("SELECT count(*) FROM public.datawizz_csv_export_test", &[])
            .await
            .expect("no writes")
            .get(0);
        assert_eq!(exists, 3);
        client
            .batch_execute("DROP TABLE public.datawizz_csv_export_test")
            .await
            .expect("fixture cleanup");
    }

    #[tokio::test]
    async fn snapshot_export_rejects_more_than_ten_thousand_rows() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_csv_export_quota (id integer PRIMARY KEY);
             INSERT INTO public.datawizz_csv_export_quota
             SELECT generate_series(1, 10001);",
            )
            .await
            .expect("large fixture");
        let result = table_export(
            State(state),
            Query(ExportRequest {
                schema: "public".into(),
                table: "datawizz_csv_export_quota".into(),
                filter_column: None,
                filter: None,
                filter2_column: None,
                filter2: None,
                sort_column: None,
                sort_dir: None,
            }),
        )
        .await;
        assert!(matches!(result, Err((StatusCode::PAYLOAD_TOO_LARGE, _))));
        client
            .batch_execute("DROP TABLE public.datawizz_csv_export_quota")
            .await
            .expect("fixture cleanup");
    }

    #[test]
    fn access_token_is_opt_in_and_uses_exact_bearer_header() {
        let secret = "0123456789abcdefghijklmnopqrstuvwxyz";
        assert!(authorized(None, None));
        assert!(!authorized(Some(secret), None));
        assert!(!authorized(Some(secret), Some("Bearer incorrect")));
        assert!(!authorized(Some(secret), Some("Basic other")));
        assert!(authorized(
            Some(secret),
            Some("Bearer 0123456789abcdefghijklmnopqrstuvwxyz")
        ));
    }

    #[test]
    fn tls_requires_verified_configuration() {
        assert!(verified_pg_tls("disable", None).unwrap().is_none());
        assert!(verified_pg_tls("disable", Some("unused")).is_err());
        assert!(verified_pg_tls("insecure", None).is_err());
        assert!(verified_pg_tls("verify-full", Some("/missing/datawizz/ca.pem")).is_err());
        assert!(verified_pg_tls("verify-full", None).unwrap().is_some());
    }

    #[test]
    fn profile_config_rejects_unsafe_names_duplicates_and_prod_writes() {
        assert!(configured_profiles("[]").unwrap().is_empty());
        assert!(configured_profiles(
            r#"[{"id":"qa","label":"QA","url":"postgres://u:secret@db/q","environment":"test"}]"#
        )
        .is_ok());
        for raw in [
            r#"[{"id":"../unsafe","label":"QA","url":"db","environment":"test"}]"#,
            r#"[{"id":"default","label":"QA","url":"db","environment":"test"}]"#,
            r#"[{"id":"qa","label":"QA","url":"db","environment":"test"},{"id":"qa","label":"Q2","url":"db","environment":"test"}]"#,
            r#"[{"id":"prod","label":"Prod","url":"db","environment":"prod","read_only":false,"tls":"verify-full"}]"#,
            r#"[{"id":"prod","label":"Prod","url":"db","environment":"prod"}]"#,
            r#"[{"id":"qa","label":"postgres://user:password@db","url":"db","environment":"test"}]"#,
        ] {
            assert!(
                configured_profiles(raw).is_err(),
                "unsafe profile config accepted"
            );
        }
        let safe = configured_profiles(r#"[{"id":"prod","label":"Production","url":"db","environment":"prod","tls":"verify-full"}]"#).unwrap();
        assert!(safe[0].read_only);
    }

    #[tokio::test]
    async fn relationships_are_read_only_and_catalog_visibility_limited() {
        let state = test_state(200, 10_000).expect("test state");
        let client = connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_er_parent (id integer PRIMARY KEY);
             CREATE TABLE public.datawizz_er_child
                 (id integer PRIMARY KEY, parent integer REFERENCES public.datawizz_er_parent(id));"
        ).await.expect("create fk fixture");
        let Json(edges) = relationships(State(state.clone()))
            .await
            .expect("load fk edges");
        let edge = edges
            .iter()
            .find(|r| r.from_table == "datawizz_er_child")
            .expect("FK must be discoverable");
        assert_eq!(edge.to_table, "datawizz_er_parent");
        assert!(edge.definition.contains("FOREIGN KEY"));
        client
            .batch_execute(
                "DROP TABLE public.datawizz_er_child;
                              DROP TABLE public.datawizz_er_parent",
            )
            .await
            .expect("cleanup");
        let mut limited = state;
        limited.read_only = true;
        assert!(connect(&limited)
            .await
            .expect("read-only connection")
            .batch_execute("CREATE TABLE public.datawizz_er_forbidden (id integer)")
            .await
            .is_err());
        let Err((status, _)) = insert_row(
            State(limited),
            Json(InsertRow {
                schema: "public".into(),
                table: "missing".into(),
                fields: vec![],
            }),
        )
        .await
        else {
            panic!("Read-only profile must reject INSERT");
        };
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn connection_options_do_not_expose_credentials() {
        let state = test_state(200, 10_000).expect("test PostgreSQL state");
        let Json(profiles) = connections(State(state)).await;
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "default");
        let body = serde_json::to_string(&profiles).expect("serialize options");
        assert!(!body.contains("postgres://"));
        assert!(!body.contains("password"));
    }

    #[tokio::test]
    async fn admin_inventory_is_read_only_and_omits_sensitive_fields() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let Json(overview) = admin_overview(State(state))
            .await
            .expect("read admin catalog");
        assert!(!overview.databases.is_empty());
        assert!(!overview.roles.is_empty());
        assert!(!overview.sessions.is_empty());
        assert!(!overview.extensions.is_empty());
        let json = serde_json::to_string(&overview).expect("serialize admin overview");
        for hidden in ["rolpassword", "client_addr", "\"query\"", "DATABASE_URL"] {
            assert!(
                !json.contains(hidden),
                "admin response must not expose {hidden}"
            );
        }
    }

    #[tokio::test]
    async fn admin_details_cover_schemas_constraints_indexes_and_safe_settings() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "DROP SCHEMA IF EXISTS datawizz_admin_test CASCADE;
             CREATE SCHEMA datawizz_admin_test;
             CREATE TABLE datawizz_admin_test.items (id integer PRIMARY KEY, label text);
             CREATE INDEX items_label_idx ON datawizz_admin_test.items (label);
             CREATE VIEW datawizz_admin_test.items_view AS
                 SELECT id, label FROM datawizz_admin_test.items;",
            )
            .await
            .expect("create admin catalog fixtures");

        let Json(admin) = admin_overview(State(state))
            .await
            .expect("read admin details");
        assert!(admin
            .schemas
            .iter()
            .any(|s| s.name == "datawizz_admin_test"));
        assert!(admin
            .relations
            .iter()
            .any(|r| r.schema == "datawizz_admin_test" && r.name == "items" && r.kind == "table"));
        assert!(admin
            .indexes
            .iter()
            .any(|i| i.schema == "datawizz_admin_test" && i.name == "items_label_idx" && i.valid));
        assert!(admin
            .constraints
            .iter()
            .any(|c| c.schema == "datawizz_admin_test" && c.name == "items_pkey" && c.kind == "p"));
        let index = admin
            .indexes
            .iter()
            .find(|i| i.schema == "datawizz_admin_test" && i.name == "items_label_idx")
            .expect("test index");
        assert!(index.definition.contains("CREATE INDEX items_label_idx"));
        let view = admin
            .relations
            .iter()
            .find(|r| r.schema == "datawizz_admin_test" && r.name == "items_view")
            .expect("test view");
        assert_eq!(view.kind, "view");
        assert!(view
            .definition
            .as_ref()
            .is_some_and(|d| d.contains("FROM datawizz_admin_test.items")));
        assert!(admin
            .relations
            .iter()
            .find(|r| r.schema == "datawizz_admin_test" && r.name == "items")
            .is_some_and(|r| r.definition.is_none()));
        assert!(admin
            .table_stats
            .iter()
            .any(|t| t.schema == "datawizz_admin_test" && t.table == "items"));
        assert!(admin.settings.iter().any(|s| s.name == "max_connections"));
        assert!(admin.settings.iter().all(|s| matches!(
            s.name.as_str(),
            "server_version"
                | "max_connections"
                | "shared_buffers"
                | "work_mem"
                | "maintenance_work_mem"
                | "statement_timeout"
                | "autovacuum"
                | "max_wal_size"
                | "wal_level"
                | "idle_in_transaction_session_timeout"
        )));
        let serialized = serde_json::to_string(&admin).expect("serialize inventory");
        for secret in ["rolpassword", "client_addr", "\"query\"", "ssl_key_file"] {
            assert!(
                !serialized.contains(secret),
                "unexpected sensitive admin field: {secret}"
            );
        }
        client
            .batch_execute("DROP SCHEMA datawizz_admin_test CASCADE")
            .await
            .expect("cleanup admin fixtures");
    }

    fn test_state(max_rows: usize, timeout_ms: u64) -> Option<AppState> {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL is required to run PostgreSQL integration tests");
        Some(AppState {
            url: Arc::new(url),
            max_rows,
            timeout_ms,
            tls: None,
            access_token: None,
            read_only: false,
            managed: None,
            profiles: Arc::new(vec![ConnectionOption {
                id: "default".into(),
                label: "Default PostgreSQL".into(),
                engine: "postgresql".into(),
                environment: "dev".into(),
                read_only: false,
            }]),
        })
    }

    #[tokio::test]
    async fn postgres_restricted_role_enforces_crud_privileges() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        // No login role or credentials are created; SET ROLE limits this one connection.
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_permissions_test (
                id integer PRIMARY KEY, note text NOT NULL
             );
             INSERT INTO public.datawizz_permissions_test VALUES (1, 'before');
             CREATE ROLE datawizz_permissions_reader NOLOGIN;
             GRANT USAGE ON SCHEMA public TO datawizz_permissions_reader;
             GRANT SELECT ON public.datawizz_permissions_test TO datawizz_permissions_reader;
             SET ROLE datawizz_permissions_reader",
            )
            .await
            .expect("setup restricted role");
        let read = client
            .query(
                "SELECT note FROM public.datawizz_permissions_test WHERE id=1",
                &[],
            )
            .await
            .expect("SELECT should be allowed");
        assert_eq!(read[0].get::<_, String>(0), "before");
        let forbidden_statements = [
            "INSERT INTO public.datawizz_permissions_test VALUES (2, 'new')",
            "UPDATE public.datawizz_permissions_test SET note='changed' WHERE id=1",
            "DELETE FROM public.datawizz_permissions_test WHERE id=1",
        ];
        for statement in forbidden_statements {
            let error = client
                .batch_execute(statement)
                .await
                .expect_err("write must be denied to the read-only role");
            assert_eq!(error.code().map(|code| code.code()), Some("42501"));
        }
        client
            .batch_execute(
                "RESET ROLE;
             REVOKE ALL ON public.datawizz_permissions_test FROM datawizz_permissions_reader;
             REVOKE USAGE ON SCHEMA public FROM datawizz_permissions_reader;
             DROP ROLE datawizz_permissions_reader;
             DROP TABLE public.datawizz_permissions_test",
            )
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn crud_handles_composite_keys_and_quoted_identifiers() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.\"Datawizz \"\"Pairs\"\"\" (
                \"Left Key\" integer NOT NULL,
                \"Right Key\" integer NOT NULL,
                \"Payload \"\"Text\"\"\" text,
                PRIMARY KEY (\"Left Key\", \"Right Key\")
            );
            INSERT INTO public.\"Datawizz \"\"Pairs\"\"\" VALUES (1,1,'first'),(1,2,'second')",
            )
            .await
            .expect("setup quoted table");
        let table = "Datawizz \"Pairs\"".to_string();
        let Json(page) = table_rows(
            State(state.clone()),
            Query(PageRequest {
                schema: "public".into(),
                table: table.clone(),
                offset: Some(0),
                limit: Some(10),
                filter_column: None,
                filter: None,
                filter2_column: None,
                filter2: None,
                sort_column: None,
                sort_dir: None,
            }),
        )
        .await
        .expect("browse quoted table");
        assert_eq!(page.rows.len(), 2);
        assert!(page.stable_order);
        assert_eq!(page.rows[1][2].as_deref(), Some("second"));
        let old = vec![Some("1".into()), Some("2".into()), Some("second".into())];
        let new = vec![Some("1".into()), Some("2".into()), Some("updated".into())];
        change_row(
            State(state.clone()),
            Json(RowChange {
                schema: "public".into(),
                table: table.clone(),
                original: old.clone(),
                values: Some(new.clone()),
                delete: false,
            }),
        )
        .await
        .expect("update composite key row");
        let Err((status, _)) = change_row(
            State(state.clone()),
            Json(RowChange {
                schema: "public".into(),
                table: table.clone(),
                original: old,
                values: Some(new.clone()),
                delete: false,
            }),
        )
        .await
        else {
            panic!("stale update must be rejected")
        };
        assert_eq!(status, StatusCode::CONFLICT);
        let Json(inserted) = insert_row(
            State(state.clone()),
            Json(InsertRow {
                schema: "public".into(),
                table: table.clone(),
                fields: vec![
                    InsertField {
                        column: "Left Key".into(),
                        value: Some("2".into()),
                    },
                    InsertField {
                        column: "Right Key".into(),
                        value: Some("1".into()),
                    },
                    InsertField {
                        column: "Payload \"Text\"".into(),
                        value: None,
                    },
                ],
            }),
        )
        .await
        .expect("insert quoted columns");
        assert_eq!(inserted.affected, 1);
        change_row(
            State(state),
            Json(RowChange {
                schema: "public".into(),
                table: table.clone(),
                original: new,
                values: None,
                delete: true,
            }),
        )
        .await
        .expect("delete one composite key row");
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM public.\"Datawizz \"\"Pairs\"\"\"",
                &[],
            )
            .await
            .expect("count")
            .get(0);
        assert_eq!(count, 2);
        client
            .batch_execute("DROP TABLE public.\"Datawizz \"\"Pairs\"\"\"")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn crud_rejects_unauthorized_columns_and_duplicate_insert_fields() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_insert_validation (
              id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
              value text NOT NULL DEFAULT 'default'
            )",
            )
            .await
            .expect("setup");
        let duplicate = insert_row(
            State(state.clone()),
            Json(InsertRow {
                schema: "public".into(),
                table: "datawizz_insert_validation".into(),
                fields: vec![
                    InsertField {
                        column: "value".into(),
                        value: Some("a".into()),
                    },
                    InsertField {
                        column: "value".into(),
                        value: Some("b".into()),
                    },
                ],
            }),
        )
        .await;
        assert!(duplicate.is_err(), "duplicate columns must not be accepted");
        let generated = insert_row(
            State(state.clone()),
            Json(InsertRow {
                schema: "public".into(),
                table: "datawizz_insert_validation".into(),
                fields: vec![InsertField {
                    column: "id".into(),
                    value: Some("100".into()),
                }],
            }),
        )
        .await;
        assert!(generated.is_err(), "identity column must use DEFAULT");
        let not_null = insert_row(
            State(state),
            Json(InsertRow {
                schema: "public".into(),
                table: "datawizz_insert_validation".into(),
                fields: vec![InsertField {
                    column: "value".into(),
                    value: None,
                }],
            }),
        )
        .await;
        assert!(not_null.is_err(), "SQL NULL must not bypass constraints");
        client
            .batch_execute("DROP TABLE public.datawizz_insert_validation")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn insert_uses_defaults_and_preserves_null() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_insert_test (
              id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
              title text NOT NULL DEFAULT 'default title',
              note text DEFAULT 'default note'
             )",
            )
            .await
            .expect("create");
        let insert = |fields| InsertRow {
            schema: "public".into(),
            table: "datawizz_insert_test".into(),
            fields,
        };
        insert_row(State(state.clone()), Json(insert(vec![])))
            .await
            .expect("defaults");
        insert_row(
            State(state.clone()),
            Json(insert(vec![
                InsertField {
                    column: "title".into(),
                    value: Some("literal NULL".into()),
                },
                InsertField {
                    column: "note".into(),
                    value: None,
                },
            ])),
        )
        .await
        .expect("explicit null");
        let rows = client
            .query(
                "SELECT id,title,note FROM public.datawizz_insert_test ORDER BY id",
                &[],
            )
            .await
            .expect("read");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<_, String>(1), "default title");
        assert_eq!(
            rows[0].get::<_, Option<String>>(2),
            Some("default note".into())
        );
        assert_eq!(rows[1].get::<_, String>(1), "literal NULL");
        assert_eq!(rows[1].get::<_, Option<String>>(2), None);
        assert!(insert_row(
            State(state.clone()),
            Json(insert(vec![InsertField {
                column: "id".into(),
                value: Some("99".into())
            }]))
        )
        .await
        .is_err());
        assert!(insert_row(
            State(state),
            Json(insert(vec![InsertField {
                column: "missing".into(),
                value: Some("a".into())
            }]))
        )
        .await
        .is_err());
        client
            .batch_execute("DROP TABLE public.datawizz_insert_test")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn changeset_rolls_back_all_rows_on_stale_conflict() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_changeset_test (id integer PRIMARY KEY, label text);
             INSERT INTO public.datawizz_changeset_test VALUES (1,'first'),(2,'second')",
            )
            .await
            .expect("seed");
        let change = |id: &str, old: &str, new: &str| ChangesetItem {
            original: vec![Some(id.into()), Some(old.into())],
            values: vec![Some(id.into()), Some(new.into())],
        };
        let request = |changes| ChangesetRequest {
            schema: "public".into(),
            table: "datawizz_changeset_test".into(),
            changes,
        };
        let Err((status, _)) = apply_changeset(
            State(state.clone()),
            Json(request(vec![
                change("1", "first", "updated"),
                change("2", "stale", "changed"),
            ])),
        )
        .await
        else {
            panic!("stale row must abort changeset")
        };
        assert_eq!(status, StatusCode::CONFLICT);
        let rows = client
            .query(
                "SELECT label FROM public.datawizz_changeset_test ORDER BY id",
                &[],
            )
            .await
            .expect("read after rollback");
        assert_eq!(rows[0].get::<_, String>(0), "first");
        assert_eq!(rows[1].get::<_, String>(0), "second");
        let Json(result) = apply_changeset(
            State(state),
            Json(request(vec![
                change("1", "first", "updated"),
                change("2", "second", "changed"),
            ])),
        )
        .await
        .expect("commit both rows");
        assert_eq!(result.affected, 2);
        let rows = client
            .query(
                "SELECT label FROM public.datawizz_changeset_test ORDER BY id",
                &[],
            )
            .await
            .expect("read after commit");
        assert_eq!(rows[0].get::<_, String>(0), "updated");
        assert_eq!(rows[1].get::<_, String>(0), "changed");
        client
            .batch_execute("DROP TABLE public.datawizz_changeset_test")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn row_edit_respects_pk_null_and_optimistic_conflicts() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_row_edit_test (
              id integer PRIMARY KEY, label text, detail text);
             INSERT INTO public.datawizz_row_edit_test VALUES (1, 'before', NULL)",
            )
            .await
            .expect("seed");
        let make = |original: Vec<Option<String>>, values: Option<Vec<Option<String>>>, delete| {
            RowChange {
                schema: "public".into(),
                table: "datawizz_row_edit_test".into(),
                original,
                values,
                delete,
            }
        };
        let original = vec![Some("1".into()), Some("before".into()), None];
        let updated = vec![Some("1".into()), Some("after".into()), Some("NULL".into())];
        let Json(success) = change_row(
            State(state.clone()),
            Json(make(original.clone(), Some(updated.clone()), false)),
        )
        .await
        .expect("update");
        assert_eq!(success.affected, 1);
        let row = client
            .query_one(
                "SELECT label,detail FROM public.datawizz_row_edit_test WHERE id=1",
                &[],
            )
            .await
            .expect("read");
        assert_eq!(row.get::<_, String>(0), "after");
        assert_eq!(row.get::<_, Option<String>>(1), Some("NULL".into()));
        let Err((code, _)) = change_row(
            State(state.clone()),
            Json(make(original, Some(updated.clone()), false)),
        )
        .await
        else {
            panic!("stale update must fail")
        };
        assert_eq!(code, StatusCode::CONFLICT);
        let Err((code, _)) = change_row(
            State(state.clone()),
            Json(make(
                updated.clone(),
                Some(vec![
                    Some("2".into()),
                    Some("after".into()),
                    Some("NULL".into()),
                ]),
                false,
            )),
        )
        .await
        else {
            panic!("changing PK must fail")
        };
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let Json(deleted) = change_row(State(state), Json(make(updated, None, true)))
            .await
            .expect("delete");
        assert_eq!(deleted.affected, 1);
        client
            .batch_execute("DROP TABLE public.datawizz_row_edit_test")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn mutation_endpoints_immediately_visible_on_next_table_fetch() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_live_edit_test (id integer PRIMARY KEY, label text);
             INSERT INTO public.datawizz_live_edit_test VALUES (1, 'before')",
            )
            .await
            .expect("seed");
        let page = || PageRequest {
            schema: "public".into(),
            table: "datawizz_live_edit_test".into(),
            offset: Some(0),
            limit: Some(20),
            filter_column: None,
            filter: None,
            filter2_column: None,
            filter2: None,
            sort_column: None,
            sort_dir: None,
        };
        let Json(initial) = table_rows(State(state.clone()), Query(page()))
            .await
            .expect("initial");
        assert_eq!(initial.rows[0][1].as_deref(), Some("before"));
        let original = vec![Some("1".into()), Some("before".into())];
        let changed = vec![Some("1".into()), Some("after".into())];
        change_row(
            State(state.clone()),
            Json(RowChange {
                schema: "public".into(),
                table: "datawizz_live_edit_test".into(),
                original: original.clone(),
                values: Some(changed.clone()),
                delete: false,
            }),
        )
        .await
        .expect("update");
        let Json(updated) = table_rows(State(state.clone()), Query(page()))
            .await
            .expect("refetch update");
        assert_eq!(updated.rows[0][1].as_deref(), Some("after"));
        let Err((code, _)) = change_row(
            State(state.clone()),
            Json(RowChange {
                schema: "public".into(),
                table: "datawizz_live_edit_test".into(),
                original: original.clone(),
                values: Some(changed.clone()),
                delete: false,
            }),
        )
        .await
        else {
            panic!("stale update must conflict")
        };
        assert_eq!(code, StatusCode::CONFLICT);
        let Json(still_updated) = table_rows(State(state.clone()), Query(page()))
            .await
            .expect("refetch after conflict");
        assert_eq!(still_updated.rows[0][1].as_deref(), Some("after"));
        insert_row(
            State(state.clone()),
            Json(InsertRow {
                schema: "public".into(),
                table: "datawizz_live_edit_test".into(),
                fields: vec![
                    InsertField {
                        column: "id".into(),
                        value: Some("2".into()),
                    },
                    InsertField {
                        column: "label".into(),
                        value: Some("inserted".into()),
                    },
                ],
            }),
        )
        .await
        .expect("insert");
        let Json(inserted) = table_rows(State(state.clone()), Query(page()))
            .await
            .expect("refetch insert");
        assert_eq!(inserted.rows.len(), 2);
        assert_eq!(inserted.rows[1][1].as_deref(), Some("inserted"));
        change_row(
            State(state.clone()),
            Json(RowChange {
                schema: "public".into(),
                table: "datawizz_live_edit_test".into(),
                original: changed,
                values: None,
                delete: true,
            }),
        )
        .await
        .expect("delete");
        let Json(deleted) = table_rows(State(state.clone()), Query(page()))
            .await
            .expect("refetch delete");
        assert_eq!(deleted.rows.len(), 1);
        assert_eq!(deleted.rows[0][0].as_deref(), Some("2"));
        client
            .batch_execute("DROP TABLE public.datawizz_live_edit_test")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn optimistic_edit_accepts_numeric_scale_and_boolean_wire_formats() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_typed_edit_test (
                id integer PRIMARY KEY, title text, price numeric(10,2), enabled boolean);
             INSERT INTO public.datawizz_typed_edit_test VALUES (1,'Mechanical keyboard',89.00,true)"
        ).await.expect("seed");
        let req = PageRequest {
            schema: "public".into(),
            table: "datawizz_typed_edit_test".into(),
            offset: Some(0),
            limit: Some(10),
            filter_column: None,
            filter: None,
            filter2_column: None,
            filter2: None,
            sort_column: None,
            sort_dir: None,
        };
        let Json(page) = table_rows(State(state.clone()), Query(req))
            .await
            .expect("load row");
        assert_eq!(page.rows[0][2].as_deref(), Some("89.00"));
        assert_eq!(page.rows[0][3].as_deref(), Some("t"));
        let old = page.rows[0].clone();
        let mut changed = old.clone();
        changed[1] = Some("Mechanical keyboard updated".into());
        change_row(
            State(state.clone()),
            Json(RowChange {
                schema: "public".into(),
                table: "datawizz_typed_edit_test".into(),
                original: old.clone(),
                values: Some(changed.clone()),
                delete: false,
            }),
        )
        .await
        .expect("typed snapshot update must succeed");
        let row = client
            .query_one(
                "SELECT title,price,enabled FROM public.datawizz_typed_edit_test WHERE id=1",
                &[],
            )
            .await
            .expect("read");
        assert_eq!(row.get::<_, String>(0), "Mechanical keyboard updated");
        assert!(row.get::<_, bool>(2));
        let Err((status, _)) = change_row(
            State(state.clone()),
            Json(RowChange {
                schema: "public".into(),
                table: "datawizz_typed_edit_test".into(),
                original: old,
                values: Some(changed.clone()),
                delete: false,
            }),
        )
        .await
        else {
            panic!("stale typed snapshot must conflict")
        };
        assert_eq!(status, StatusCode::CONFLICT);
        let mut batch = changed.clone();
        batch[1] = Some("Batch updated".into());
        let Json(saved) = apply_changeset(
            State(state),
            Json(ChangesetRequest {
                schema: "public".into(),
                table: "datawizz_typed_edit_test".into(),
                changes: vec![ChangesetItem {
                    original: changed,
                    values: batch,
                }],
            }),
        )
        .await
        .expect("typed changeset should succeed");
        assert_eq!(saved.affected, 1);
        client
            .batch_execute("DROP TABLE public.datawizz_typed_edit_test")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn table_pagination_reads_multiple_pages() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        // Use a unique schema/object name for deterministic test isolation.
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS public.datawizz_paging_test (id int PRIMARY KEY);
             TRUNCATE public.datawizz_paging_test;
             INSERT INTO public.datawizz_paging_test SELECT generate_series(1, 205)",
            )
            .await
            .expect("seed");
        let make = |offset| PageRequest {
            schema: "public".into(),
            table: "datawizz_paging_test".into(),
            offset: Some(offset),
            limit: Some(100),
            filter_column: None,
            filter: None,
            filter2_column: None,
            filter2: None,
            sort_column: None,
            sort_dir: None,
        };
        let Json(first) = table_rows(State(state.clone()), Query(make(0)))
            .await
            .expect("first");
        let Json(second) = table_rows(State(state.clone()), Query(make(100)))
            .await
            .expect("second");
        let Json(third) = table_rows(State(state.clone()), Query(make(200)))
            .await
            .expect("third");
        assert_eq!(first.rows.len(), 100);
        assert_eq!(first.rows[0][0].as_deref(), Some("1"));
        assert!(first.has_next && first.stable_order);
        assert_eq!(second.rows[0][0].as_deref(), Some("101"));
        assert_eq!(third.rows.len(), 5);
        assert!(!third.has_next);
        assert!(table_rows(State(state), Query(make(-1))).await.is_err());
        client
            .batch_execute("DROP TABLE public.datawizz_paging_test")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn server_table_filter_sort_handles_escaped_text_and_rejects_columns() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_filter_sort_test
                (id integer PRIMARY KEY, label text, active boolean);
             INSERT INTO public.datawizz_filter_sort_test VALUES
                (1, 'Alpha_One', true), (2, 'Beta%Two', false),
                (3, 'Alpha_X', true), (4, 'alpha_One', false);",
            )
            .await
            .expect("setup");
        let page = |value: &str, sort_dir: &str, offset: i64| PageRequest {
            schema: "public".into(),
            table: "datawizz_filter_sort_test".into(),
            offset: Some(offset),
            limit: Some(2),
            filter_column: Some("label".into()),
            filter: Some(value.into()),
            filter2_column: None,
            filter2: None,
            sort_column: Some("id".into()),
            sort_dir: Some(sort_dir.into()),
        };
        let Json(first) = table_rows(State(state.clone()), Query(page("alpha_", "desc", 0)))
            .await
            .expect("filtered page");
        assert_eq!(first.rows.len(), 2);
        assert!(first.has_next && first.stable_order);
        assert_eq!(first.rows[0][0].as_deref(), Some("4"));
        assert_eq!(first.rows[1][0].as_deref(), Some("3"));
        let Json(second) = table_rows(State(state.clone()), Query(page("alpha_", "desc", 2)))
            .await
            .expect("second filtered page");
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.rows[0][0].as_deref(), Some("1"));
        assert!(!second.has_next);
        let Json(percent) = table_rows(State(state.clone()), Query(page("%", "asc", 0)))
            .await
            .expect("literal percent");
        assert_eq!(percent.rows.len(), 1);
        assert_eq!(percent.rows[0][1].as_deref(), Some("Beta%Two"));
        assert_eq!(percent.rows[0][2].as_deref(), Some("f"));
        let mut invalid = page("x", "asc", 0);
        invalid.filter_column = Some("label) OR TRUE--".into());
        assert_eq!(
            table_rows(State(state.clone()), Query(invalid))
                .await
                .err()
                .expect("invalid column must fail")
                .0,
            StatusCode::BAD_REQUEST
        );
        let mut invalid = page("x", "desc; DROP TABLE x", 0);
        assert_eq!(
            table_rows(State(state.clone()), Query(invalid))
                .await
                .err()
                .expect("invalid direction must fail")
                .0,
            StatusCode::BAD_REQUEST
        );
        let mut invalid = page("x", "asc", 0);
        invalid.sort_column = Some("unknown".into());
        assert_eq!(
            table_rows(State(state.clone()), Query(invalid))
                .await
                .err()
                .expect("invalid sort column must fail")
                .0,
            StatusCode::BAD_REQUEST
        );
        client
            .batch_execute("DROP TABLE public.datawizz_filter_sort_test")
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn schema_columns_include_primary_key() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TEMP TABLE datawizz_pk_test (id bigint PRIMARY KEY, description text)",
            )
            .await
            .expect("create temp table");
        // The API opens separate connections, so temporary tables are not visible there.
        // Verify the catalog expression directly on the same connection.
        let rows = client
            .query(
                "SELECT a.attname, EXISTS (SELECT 1 FROM pg_catalog.pg_index i
              WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey))
             FROM pg_catalog.pg_attribute a
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
             WHERE c.oid = 'pg_temp.datawizz_pk_test'::regclass
               AND a.attnum > 0 ORDER BY a.attnum",
                &[],
            )
            .await
            .expect("inspect primary key");
        assert_eq!(rows[0].get::<_, String>(0), "id");
        assert!(rows[0].get::<_, bool>(1));
        assert!(!rows[1].get::<_, bool>(1));
    }

    #[tokio::test]
    async fn query_reports_column_types_and_empty_results() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let Json(result) = execute(
            State(state.clone()),
            Json(QueryRequest {
                sql: "SELECT 42::int4 AS id, true AS enabled, '{}'::jsonb AS payload WHERE false"
                    .into(),
            }),
        )
        .await
        .expect("valid SELECT");
        assert_eq!(result.columns, vec!["id", "enabled", "payload"]);
        assert_eq!(result.column_types, vec!["int4", "bool", "jsonb"]);
        assert!(result.rows.is_empty());
        assert!(!result.truncated);

        let Json(changed) = execute(
            State(state),
            Json(QueryRequest {
                sql: "CREATE TEMP TABLE datawizz_command_check (id integer)".into(),
            }),
        )
        .await
        .expect("valid DDL");
        assert!(changed.columns.is_empty());
        assert!(changed.column_types.is_empty());
    }

    #[tokio::test]
    async fn query_caps_rows_and_marks_truncation() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let Json(result) = execute(
            State(state),
            Json(QueryRequest {
                sql: "SELECT generate_series(1, 201) AS number".into(),
            }),
        )
        .await
        .expect("valid query");
        assert_eq!(result.columns, vec!["number"]);
        assert_eq!(result.rows.len(), 200);
        assert!(result.truncated);
        assert_eq!(result.rows[0][0].as_deref(), Some("1"));
        assert_eq!(result.rows[199][0].as_deref(), Some("200"));
    }

    #[tokio::test]
    async fn query_preserves_null_and_rejects_multiple_statements() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let Json(result) = execute(
            State(state.clone()),
            Json(QueryRequest {
                sql: "SELECT NULL::text AS empty_value, true AS flag, 42 AS number".into(),
            }),
        )
        .await
        .expect("valid query");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0],
            vec![None, Some("t".into()), Some("42".into())]
        );

        let error = execute(
            State(state),
            Json(QueryRequest {
                sql: "SELECT 1; SELECT 2".into(),
            }),
        )
        .await;
        assert!(error.is_err(), "multiple statements must be rejected");
    }

    #[tokio::test]
    async fn query_error_does_not_expose_sql_text() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let Err((status, Json(error))) = execute(
            State(state),
            Json(QueryRequest {
                sql: "SELECT confidential_marker_that_must_not_leak FROM nonexistent_table".into(),
            }),
        )
        .await
        else {
            panic!("invalid SQL must return an error");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error.error.contains("SQLSTATE:"));
        assert!(!error
            .error
            .contains("confidential_marker_that_must_not_leak"));
        assert!(!error.error.contains("nonexistent_table"));
    }

    #[tokio::test]
    async fn query_accepts_comments_dollar_quotes_and_returning() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let Json(selected) = execute(
            State(state.clone()),
            Json(QueryRequest {
                sql: "/* comment with ; */ SELECT $tag$semi;colon$tag$ AS value -- trailing ;"
                    .into(),
            }),
        )
        .await
        .expect("comments and semicolons in dollar-quoted strings are one statement");
        assert_eq!(selected.rows.len(), 1);
        assert_eq!(selected.rows[0][0].as_deref(), Some("semi;colon"));
        assert!(!selected.truncated);

        let client = connect(&state).await.expect("connect");
        client
            .batch_execute(
                "CREATE TABLE public.datawizz_returning_test (id integer PRIMARY KEY, note text)",
            )
            .await
            .expect("create returning test table");

        let Json(inserted) = execute(
            State(state.clone()),
            Json(QueryRequest {
                sql: "INSERT INTO public.datawizz_returning_test VALUES (1, 'created') RETURNING id, note".into(),
            }),
        )
        .await
        .expect("insert returning");
        assert_eq!(inserted.columns, vec!["id", "note"]);
        assert_eq!(
            inserted.rows,
            vec![vec![Some("1".into()), Some("created".into())]]
        );
        assert!(!inserted.truncated);

        let Json(updated) = execute(
            State(state.clone()),
            Json(QueryRequest {
                sql: "UPDATE public.datawizz_returning_test SET note = 'updated' WHERE id = 1 RETURNING note".into(),
            }),
        )
        .await
        .expect("update returning");
        assert_eq!(updated.rows, vec![vec![Some("updated".into())]]);

        let Json(deleted) = execute(
            State(state),
            Json(QueryRequest {
                sql: "DELETE FROM public.datawizz_returning_test WHERE id = 1 RETURNING id".into(),
            }),
        )
        .await
        .expect("delete returning");
        assert_eq!(deleted.rows, vec![vec![Some("1".into())]]);

        client
            .batch_execute("DROP TABLE public.datawizz_returning_test")
            .await
            .expect("cleanup returning test table");
    }

    #[tokio::test]
    async fn query_times_out_without_crashing() {
        let Some(state) = test_state(200, 100) else {
            return;
        };
        let error = execute(
            State(state),
            Json(QueryRequest {
                sql: "SELECT pg_sleep(1)".into(),
            }),
        )
        .await;
        assert!(error.is_err(), "slow statement should time out");
    }
}
