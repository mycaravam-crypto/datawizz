use std::{env, sync::Arc, time::Instant};

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio_postgres::{NoTls, SimpleQueryMessage};

#[derive(Clone)]
struct AppState {
    url: Arc<String>,
    max_rows: usize,
    timeout_ms: u64,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: message.into() }))
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

async fn connect(state: &AppState) -> Result<tokio_postgres::Client, (StatusCode, Json<ApiError>)> {
    // The initial MVP uses one short-lived DB connection per HTTP request.
    // No cross-user session state or connection-pool complexity.
    let (client, connection) = tokio_postgres::connect(&state.url, NoTls)
        .await
        .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "PostgreSQL connection failed"))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("PostgreSQL connection closed: {error}");
        }
    });
    client
        .batch_execute(&format!("SET statement_timeout = {}", state.timeout_ms))
        .await
        .map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Could not configure query timeout"))?;
    Ok(client)
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
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(rows.iter().map(|r| DbObject {
        schema: r.get(0),
        name: r.get(1),
        kind: r.get(2),
    }).collect()))
}

#[derive(Deserialize)]
struct ColumnRequest { schema: String, object: String }

#[derive(Serialize)]
struct ColumnInfo { name: String, data_type: String, nullable: bool }

async fn columns(
    State(state): State<AppState>,
    Query(req): Query<ColumnRequest>,
) -> ApiResult<Vec<ColumnInfo>> {
    let client = connect(&state).await?;
    let rows = client
        .query(
            "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), NOT a.attnotnull
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
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(rows.iter().map(|r| ColumnInfo {
        name: r.get(0),
        data_type: r.get(1),
        nullable: r.get(2),
    }).collect()))
}

#[derive(Deserialize)]
struct QueryRequest { sql: String }

#[derive(Serialize)]
struct QueryResponse {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    affected: u64,
    truncated: bool,
    duration_ms: u128,
}

async fn execute(State(state): State<AppState>, Json(req): Json<QueryRequest>) -> ApiResult<QueryResponse> {
    if req.sql.trim().is_empty() || req.sql.len() > 256_000 {
        return Err(api_error(StatusCode::BAD_REQUEST, "SQL is empty or exceeds 256 KB"));
    }
    let client = connect(&state).await?;
    // PostgreSQL's extended-query parser rejects multiple statements.
    // This also avoids incorrectly splitting SQL on semicolons inside strings.
    client.prepare(&req.sql).await
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    let started = Instant::now();
    let stream = client.simple_query_raw(&req.sql).await
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))?;
    tokio::pin!(stream);
    let mut result = QueryResponse {
        columns: vec![], rows: vec![], affected: 0, truncated: false, duration_ms: 0,
    };
    while let Some(message) = stream.try_next().await
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))? {
        match message {
            SimpleQueryMessage::RowDescription(columns) => {
                result.columns = columns.iter().map(|c| c.name().to_owned()).collect();
            }
            SimpleQueryMessage::Row(row) => {
                if result.columns.is_empty() {
                    result.columns = row.columns().iter().map(|c| c.name().to_owned()).collect();
                }
                if result.rows.len() < state.max_rows {
                    result.rows.push((0..row.len()).map(|i| row.get(i).map(str::to_owned)).collect());
                } else {
                    result.truncated = true;
                }
            }
            SimpleQueryMessage::CommandComplete(count) => result.affected = count,
            _ => {}
        }
    }
    result.duration_ms = started.elapsed().as_millis();
    Ok(Json(result))
}

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(include_str!("../web/index.html")))
}

#[tokio::main]
async fn main() {
    let url = env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let port: u16 = env::var("PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(3000);
    let max_rows = env::var("MAX_ROWS").ok().and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200).clamp(1, 1000);
    let timeout_ms = env::var("QUERY_TIMEOUT_MS").ok().and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10_000).clamp(100, 60_000);
    let state = AppState { url: Arc::new(url), max_rows, timeout_ms };
    // Fail fast before accepting browser traffic.
    connect(&state).await.expect("Cannot connect to PostgreSQL");
    let app = Router::new()
        .route("/", get(index))
        .route("/api/schema", get(schema))
        .route("/api/columns", get(columns))
        .route("/api/query", post(execute))
        .with_state(state);
    let address = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&address).await.expect("Cannot bind port");
    println!("Datawizz ready at http://{address}");
    axum::serve(listener, app).await.expect("HTTP server failed");
}
