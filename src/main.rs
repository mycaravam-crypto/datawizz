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

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: message.into() }))
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn postgres_error(error: &tokio_postgres::Error) -> (StatusCode, Json<ApiError>) {
    // Return a bounded PostgreSQL SQLSTATE, never server-supplied messages,
    // SQL fragments, object names or connection details.
    let code = error
        .code()
        .map(|code| code.code())
        .filter(|code| code.len() == 5 && code.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
        .unwrap_or("UNKNOWN");
    api_error(StatusCode::BAD_REQUEST, format!("PostgreSQL query failed (SQLSTATE: {code})"))
}

async fn connect(state: &AppState) -> Result<tokio_postgres::Client, (StatusCode, Json<ApiError>)> {
    // The initial MVP uses one short-lived DB connection per HTTP request.
    // No cross-user session state or connection-pool complexity.
    let (client, connection) = tokio_postgres::connect(&state.url, NoTls)
        .await
        .map_err(|error| {
            eprintln!("PostgreSQL connection failed: {error}");
            api_error(StatusCode::BAD_GATEWAY, "PostgreSQL connection failed")
        })?;
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
        .map_err(|e| postgres_error(&e))?;
    Ok(Json(rows.iter().map(|r| DbObject {
        schema: r.get(0),
        name: r.get(1),
        kind: r.get(2),
    }).collect()))
}

#[derive(Deserialize)]
struct ColumnRequest { schema: String, object: String }

#[derive(Serialize)]
struct ColumnInfo { name: String, data_type: String, nullable: bool, primary_key: bool }

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
    Ok(Json(rows.iter().map(|r| ColumnInfo {
        name: r.get(0),
        data_type: r.get(1),
        nullable: r.get(2),
        primary_key: r.get(3),
    }).collect()))
}

#[derive(Deserialize)]
struct QueryRequest { sql: String }

#[derive(Serialize)]
struct QueryResponse {
    columns: Vec<String>,
    column_types: Vec<String>,
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
    let statement = client.prepare(&req.sql).await
        .map_err(|e| postgres_error(&e))?;
    let started = Instant::now();
    let stream = client.simple_query_raw(&req.sql).await
        .map_err(|e| postgres_error(&e))?;
    tokio::pin!(stream);
    let mut result = QueryResponse {
        columns: statement.columns().iter().map(|c| c.name().to_owned()).collect(),
        column_types: statement.columns().iter().map(|c| c.type_().name().to_owned()).collect(),
        rows: vec![], affected: 0, truncated: false, duration_ms: 0,
    };
    while let Some(message) = stream.try_next().await
        .map_err(|e| postgres_error(&e))? {
        match message {
            SimpleQueryMessage::Row(row) => {
                if result.rows.len() < state.max_rows {
                    result.rows.push((0..row.len()).map(|i| row.get(i).map(str::to_owned)).collect());
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
    let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let address = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&address).await.expect("Cannot bind port");
    println!("Datawizz ready at http://{address}");
    axum::serve(listener, app).await.expect("HTTP server failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(max_rows: usize, timeout_ms: u64) -> Option<AppState> {
        let url = std::env::var("DATABASE_URL").ok()?;
        Some(AppState {
            url: Arc::new(url),
            max_rows,
            timeout_ms,
        })
    }

    #[tokio::test]
    async fn schema_columns_include_primary_key() {
        let Some(state) = test_state(200, 10_000) else { return; };
        let client = connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TEMP TABLE datawizz_pk_test (id bigint PRIMARY KEY, description text)"
        ).await.expect("create temp table");
        // The API opens separate connections, so temporary tables are not visible there.
        // Verify the catalog expression directly on the same connection.
        let rows = client.query(
            "SELECT a.attname, EXISTS (SELECT 1 FROM pg_catalog.pg_index i
              WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey))
             FROM pg_catalog.pg_attribute a
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
             WHERE c.oid = 'pg_temp.datawizz_pk_test'::regclass
               AND a.attnum > 0 ORDER BY a.attnum", &[]
        ).await.expect("inspect primary key");
        assert_eq!(rows[0].get::<_, String>(0), "id");
        assert!(rows[0].get::<_, bool>(1));
        assert!(!rows[1].get::<_, bool>(1));
    }

    #[tokio::test]
    async fn query_reports_column_types_and_empty_results() {
        let Some(state) = test_state(200, 10_000) else { return; };
        let Json(result) = execute(State(state.clone()), Json(QueryRequest {
            sql: "SELECT 42::int4 AS id, true AS enabled, '{}'::jsonb AS payload WHERE false".into()
        })).await.expect("valid SELECT");
        assert_eq!(result.columns, vec!["id", "enabled", "payload"]);
        assert_eq!(result.column_types, vec!["int4", "bool", "jsonb"]);
        assert!(result.rows.is_empty());
        assert!(!result.truncated);

        let Json(changed) = execute(State(state), Json(QueryRequest {
            sql: "CREATE TEMP TABLE datawizz_command_check (id integer)".into()
        })).await.expect("valid DDL");
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
        assert_eq!(result.rows[0], vec![None, Some("t".into()), Some("42".into())]);

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
        ).await else {
            panic!("invalid SQL must return an error");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error.error.contains("SQLSTATE:"));
        assert!(!error.error.contains("confidential_marker_that_must_not_leak"));
        assert!(!error.error.contains("nonexistent_table"));
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
