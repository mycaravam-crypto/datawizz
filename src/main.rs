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


#[derive(Deserialize)]
struct PageRequest {
    schema: String,
    table: String,
    offset: Option<i64>,
    limit: Option<i64>,
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

async fn table_rows(
    State(state): State<AppState>,
    Query(req): Query<PageRequest>,
) -> ApiResult<PageResponse> {
    let offset = req.offset.unwrap_or(0);
    let limit = req.limit.unwrap_or(100);
    if !(0..=1_000_000).contains(&offset) || !(1..=200).contains(&limit) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Invalid offset or page size"));
    }
    let client = connect(&state).await?;
    // Resolve the relation using bound parameters, then use only names from pg_catalog
    // when constructing safely quoted SQL identifiers.
    let relation = client.query_opt(
        "SELECT c.oid, n.nspname, c.relname
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2
           AND c.relkind IN ('r','p','v','m','f')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'SELECT')",
        &[&req.schema, &req.table],
    ).await.map_err(|e| postgres_error(&e))?
     .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Table not found or access denied"))?;
    let oid: u32 = relation.get(0);
    let schema: String = relation.get(1);
    let table: String = relation.get(2);
    let keys = client.query(
        "SELECT a.attname FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_attribute a ON a.attrelid=i.indrelid
            AND a.attnum = ANY(i.indkey)
         WHERE i.indrelid=$1 AND i.indisprimary
         ORDER BY array_position(i.indkey::smallint[],a.attnum::smallint)",
        &[&oid],
    ).await.map_err(|e| postgres_error(&e))?;
    let stable_order = !keys.is_empty();
    let order = if stable_order {
        format!(" ORDER BY {}", keys.iter().map(|row| {
            let name: String = row.get(0);
            quote_ident(&name)
        }).collect::<Vec<_>>().join(", "))
    } else { String::new() };
    let sql = format!(
        "SELECT * FROM {}.{}{} LIMIT {} OFFSET {}",
        quote_ident(&schema), quote_ident(&table), order, limit+1, offset
    );
    let statement = client.prepare(&sql).await.map_err(|e| postgres_error(&e))?;
    let columns = statement.columns().iter().map(|c| c.name().to_string()).collect();
    let stream = client.simple_query_raw(&sql).await.map_err(|e| postgres_error(&e))?;
    tokio::pin!(stream);
    let mut rows = Vec::new();
    while let Some(message) = stream.try_next().await.map_err(|e| postgres_error(&e))? {
        if let SimpleQueryMessage::Row(row) = message {
            rows.push((0..row.len()).map(|i| row.get(i).map(str::to_owned)).collect());
        }
    }
    let has_next = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(Json(PageResponse { columns, rows, offset, limit, has_next, stable_order }))
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
struct ChangeResult { affected: u64 }

async fn change_row(State(state): State<AppState>, Json(req): Json<RowChange>) -> ApiResult<ChangeResult> {
    let mut client = connect(&state).await?;
    // Only base/partitioned tables, never views, foreign tables or relations without a PK.
    let relation = client.query_opt(
        "SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind IN ('r','p')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'SELECT')
           AND has_table_privilege(c.oid,CASE WHEN $3 THEN 'DELETE' ELSE 'UPDATE' END)",
        &[&req.schema, &req.table, &req.delete],
    ).await.map_err(|e| postgres_error(&e))?
      .ok_or_else(|| api_error(StatusCode::FORBIDDEN, "Table is not editable or access denied"))?;
    let oid: u32 = relation.get(0);
    let schema: String = relation.get(1);
    let table: String = relation.get(2);
    let columns = client.query(
        "SELECT a.attname, pg_catalog.format_type(a.atttypid,a.atttypmod),
                EXISTS (SELECT 1 FROM pg_catalog.pg_index i
                        WHERE i.indrelid=$1 AND i.indisprimary
                        AND a.attnum = ANY(i.indkey)),
                a.attgenerated <> '' OR a.attidentity <> ''
         FROM pg_catalog.pg_attribute a
         WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped
         ORDER BY a.attnum",
        &[&oid],
    ).await.map_err(|e| postgres_error(&e))?;
    if columns.is_empty() || !columns.iter().any(|c| c.get::<_, bool>(2)) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Primary key required"));
    }
    if req.original.len()!=columns.len() || (!req.delete && req.values.as_ref().map(Vec::len)!=Some(columns.len())) {
        return Err(api_error(StatusCode::BAD_REQUEST, "Row columns mismatch"));
    }
    let mut args: Vec<Option<String>> = Vec::new();
    let mut where_parts = Vec::new();
    for (i,c) in columns.iter().enumerate() {
        // Compare the full originally loaded row to reject concurrent modifications.
        let name: String = c.get(0);
        args.push(req.original[i].clone());
        where_parts.push(format!("{} IS NOT DISTINCT FROM CAST(${}::text AS {})",quote_ident(&name),args.len(),c.get::<_,String>(1)));
    }
    let qualified=format!("{}.{}",quote_ident(&schema),quote_ident(&table));
    let sql = if req.delete {
        format!("DELETE FROM {qualified} WHERE {} RETURNING 1",where_parts.join(" AND "))
    } else {
        let mut assignments=Vec::new();
        let values=req.values.as_ref().expect("checked above");
        for (i,c) in columns.iter().enumerate() {
            if values[i]==req.original[i] {continue;}
            if c.get::<_,bool>(2) || c.get::<_,bool>(3) {
                return Err(api_error(StatusCode::BAD_REQUEST,"Primary key and generated columns cannot be edited"));
            }
            let name: String=c.get(0);
            let datatype: String=c.get(1);
            args.push(values[i].clone());
            assignments.push(format!("{} = CAST(${}::text AS {})",quote_ident(&name),args.len(),datatype));
        }
        if assignments.is_empty() {
            return Err(api_error(StatusCode::BAD_REQUEST,"No changed editable fields"));
        }
        format!("UPDATE {qualified} SET {} WHERE {} RETURNING 1",assignments.join(", "),where_parts.join(" AND "))
    };
    // Execute atomically; RETURNING verifies one row before committing.
    let transaction=client.transaction().await.map_err(|e| postgres_error(&e))?;
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        args.iter().map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
    let rows=transaction.query(&sql,&refs).await.map_err(|e| postgres_error(&e))?;
    if rows.len()!=1 {
        return Err(api_error(StatusCode::CONFLICT,"Row changed or no longer exists; refresh and retry"));
    }
    transaction.commit().await.map_err(|e| postgres_error(&e))?;
    Ok(Json(ChangeResult{affected:1}))
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
    if req.changes.is_empty() || req.changes.len() > 20 {
        return Err(api_error(StatusCode::BAD_REQUEST, "Changeset must contain 1 to 20 rows"));
    }
    let mut client = connect(&state).await?;
    let relation = client.query_opt(
        "SELECT c.oid,n.nspname,c.relname FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind IN ('r','p')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'SELECT')
           AND has_table_privilege(c.oid,'UPDATE')",
        &[&req.schema,&req.table],
    ).await.map_err(|e| postgres_error(&e))?
     .ok_or_else(||api_error(StatusCode::FORBIDDEN,"Table is not editable or access denied"))?;
    let oid:u32=relation.get(0);
    let schema:String=relation.get(1);
    let table:String=relation.get(2);
    let columns=client.query(
        "SELECT a.attname,pg_catalog.format_type(a.atttypid,a.atttypmod),
                EXISTS (SELECT 1 FROM pg_catalog.pg_index i
                        WHERE i.indrelid=$1 AND i.indisprimary
                          AND a.attnum=ANY(i.indkey)),
                a.attgenerated <> '' OR a.attidentity <> ''
         FROM pg_catalog.pg_attribute a
         WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped
         ORDER BY a.attnum",
        &[&oid],
    ).await.map_err(|e| postgres_error(&e))?;
    if columns.is_empty() || !columns.iter().any(|c|c.get::<_,bool>(2)) {
        return Err(api_error(StatusCode::BAD_REQUEST,"Primary key required"));
    }
    let qualified=format!("{}.{}",quote_ident(&schema),quote_ident(&table));
    let transaction=client.transaction().await.map_err(|e|postgres_error(&e))?;
    for change in &req.changes {
        if change.original.len()!=columns.len() || change.values.len()!=columns.len() {
            return Err(api_error(StatusCode::BAD_REQUEST,"Row columns mismatch"));
        }
        let mut args:Vec<Option<String>>=Vec::new();
        let mut predicates=Vec::new();
        let mut assignments=Vec::new();
        for (i,col) in columns.iter().enumerate() {
            let name:String=col.get(0);
            args.push(change.original[i].clone());
            predicates.push(format!("{} IS NOT DISTINCT FROM CAST(${}::text AS {})",quote_ident(&name),args.len(),col.get::<_,String>(1)));
        }
        for (i,col) in columns.iter().enumerate() {
            if change.values[i]==change.original[i] {continue}
            if col.get::<_,bool>(2)||col.get::<_,bool>(3) {
                return Err(api_error(StatusCode::BAD_REQUEST,"Primary key and generated columns cannot be edited"));
            }
            let name:String=col.get(0);
            let datatype:String=col.get(1);
            args.push(change.values[i].clone());
            assignments.push(format!("{}=CAST(${}::text AS {})",quote_ident(&name),args.len(),datatype));
        }
        if assignments.is_empty() {
            return Err(api_error(StatusCode::BAD_REQUEST,"No changed editable fields"));
        }
        let sql=format!("UPDATE {qualified} SET {} WHERE {} RETURNING 1",
            assignments.join(", "),predicates.join(" AND "));
        let refs:Vec<&(dyn tokio_postgres::types::ToSql+Sync)>=args.iter()
            .map(|v|v as &(dyn tokio_postgres::types::ToSql+Sync)).collect();
        let rows=transaction.query(&sql,&refs).await.map_err(|e|postgres_error(&e))?;
        if rows.len()!=1 {
            return Err(api_error(StatusCode::CONFLICT,"Changeset conflict; no rows committed"));
        }
    }
    transaction.commit().await.map_err(|e|postgres_error(&e))?;
    Ok(Json(ChangeResult{affected:req.changes.len() as u64}))
}

#[derive(Deserialize)]
struct InsertField {
    column: String,
    // null means SQL NULL, while omitting a column uses its database DEFAULT.
    value: Option<String>,
}
#[derive(Deserialize)]
struct InsertRow { schema: String, table: String, fields: Vec<InsertField> }

async fn insert_row(State(state): State<AppState>, Json(req): Json<InsertRow>) -> ApiResult<ChangeResult> {
    let client=connect(&state).await?;
    let relation=client.query_opt(
        "SELECT c.oid,n.nspname,c.relname FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind IN ('r','p')
           AND has_schema_privilege(n.oid,'USAGE')
           AND has_table_privilege(c.oid,'INSERT')",
        &[&req.schema,&req.table],
    ).await.map_err(|e| postgres_error(&e))?
     .ok_or_else(|| api_error(StatusCode::FORBIDDEN,"Table is not insertable or access denied"))?;
    let oid:u32=relation.get(0);
    let schema:String=relation.get(1);
    let table:String=relation.get(2);
    let metadata=client.query(
        "SELECT a.attname,pg_catalog.format_type(a.atttypid,a.atttypmod),
                a.attgenerated <> '' OR a.attidentity <> ''
         FROM pg_catalog.pg_attribute a
         WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped
         ORDER BY a.attnum",&[&oid],
    ).await.map_err(|e| postgres_error(&e))?;
    if req.fields.len()>metadata.len() {
        return Err(api_error(StatusCode::BAD_REQUEST,"Too many insert fields"));
    }
    let mut names=Vec::new();
    let mut expressions=Vec::new();
    let mut values:Vec<Option<String>>=Vec::new();
    for field in &req.fields {
        let column=metadata.iter().find(|r|r.get::<_,String>(0)==field.column)
            .ok_or_else(||api_error(StatusCode::BAD_REQUEST,"Unknown insert column"))?;
        if column.get::<_,bool>(2) || names.iter().any(|n:&String|n==&field.column) {
            return Err(api_error(StatusCode::BAD_REQUEST,"Duplicate or generated insert column"));
        }
        names.push(field.column.clone());
        values.push(field.value.clone());
        let datatype:String=column.get(1);
        expressions.push(format!("CAST(${}::text AS {})",values.len(),datatype));
    }
    let qualified=format!("{}.{}",quote_ident(&schema),quote_ident(&table));
    let sql=if names.is_empty() {
        format!("INSERT INTO {qualified} DEFAULT VALUES")
    } else {
        format!("INSERT INTO {qualified} ({}) VALUES ({})",
            names.iter().map(|n|quote_ident(n)).collect::<Vec<_>>().join(", "),
            expressions.join(", "))
    };
    let refs:Vec<&(dyn tokio_postgres::types::ToSql+Sync)>=values.iter()
        .map(|v|v as &(dyn tokio_postgres::types::ToSql+Sync)).collect();
    let affected=client.execute(&sql,&refs).await.map_err(|e| postgres_error(&e))?;
    if affected!=1 {
        return Err(api_error(StatusCode::CONFLICT,"Insert did not affect exactly one row"));
    }
    Ok(Json(ChangeResult{affected}))
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
        .route("/api/table-rows", get(table_rows))
        .route("/api/row-change", post(change_row))
        .route("/api/changeset", post(apply_changeset))
        .route("/api/row-insert", post(insert_row))
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
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL is required to run PostgreSQL integration tests");
        Some(AppState {
            url: Arc::new(url),
            max_rows,
            timeout_ms,
        })
    }





    #[tokio::test]
    async fn postgres_restricted_role_enforces_crud_privileges() {
        let Some(state) = test_state(200, 10_000) else { return; };
        let client = connect(&state).await.expect("connect");
        // No login role or credentials are created; SET ROLE limits this one connection.
        client.batch_execute(
            "CREATE TABLE public.datawizz_permissions_test (
                id integer PRIMARY KEY, note text NOT NULL
             );
             INSERT INTO public.datawizz_permissions_test VALUES (1, 'before');
             CREATE ROLE datawizz_permissions_reader NOLOGIN;
             GRANT USAGE ON SCHEMA public TO datawizz_permissions_reader;
             GRANT SELECT ON public.datawizz_permissions_test TO datawizz_permissions_reader;
             SET ROLE datawizz_permissions_reader"
        ).await.expect("setup restricted role");
        let read = client.query(
            "SELECT note FROM public.datawizz_permissions_test WHERE id=1", &[]
        ).await.expect("SELECT should be allowed");
        assert_eq!(read[0].get::<_,String>(0), "before");
        let forbidden_statements = [
            "INSERT INTO public.datawizz_permissions_test VALUES (2, 'new')",
            "UPDATE public.datawizz_permissions_test SET note='changed' WHERE id=1",
            "DELETE FROM public.datawizz_permissions_test WHERE id=1",
        ];
        for statement in forbidden_statements {
            let error = client.batch_execute(statement).await
                .expect_err("write must be denied to the read-only role");
            assert_eq!(error.code().map(|code|code.code()),Some("42501"));
        }
        client.batch_execute(
            "RESET ROLE;
             REVOKE ALL ON public.datawizz_permissions_test FROM datawizz_permissions_reader;
             REVOKE USAGE ON SCHEMA public FROM datawizz_permissions_reader;
             DROP ROLE datawizz_permissions_reader;
             DROP TABLE public.datawizz_permissions_test"
        ).await.expect("cleanup");
    }

    #[tokio::test]
    async fn crud_handles_composite_keys_and_quoted_identifiers() {
        let Some(state)=test_state(200,10_000) else {return;};
        let client=connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.\"Datawizz \"\"Pairs\"\"\" (
                \"Left Key\" integer NOT NULL,
                \"Right Key\" integer NOT NULL,
                \"Payload \"\"Text\"\"\" text,
                PRIMARY KEY (\"Left Key\", \"Right Key\")
            );
            INSERT INTO public.\"Datawizz \"\"Pairs\"\"\" VALUES (1,1,'first'),(1,2,'second')"
        ).await.expect("setup quoted table");
        let table="Datawizz \"Pairs\"".to_string();
        let Json(page)=table_rows(State(state.clone()),Query(PageRequest{
            schema:"public".into(),table:table.clone(),offset:Some(0),limit:Some(10)
        })).await.expect("browse quoted table");
        assert_eq!(page.rows.len(),2);
        assert!(page.stable_order);
        assert_eq!(page.rows[1][2].as_deref(),Some("second"));
        let old=vec![Some("1".into()),Some("2".into()),Some("second".into())];
        let new=vec![Some("1".into()),Some("2".into()),Some("updated".into())];
        change_row(State(state.clone()),Json(RowChange{
            schema:"public".into(),table:table.clone(),
            original:old.clone(),values:Some(new.clone()),delete:false
        })).await.expect("update composite key row");
        let Err((status,_))=change_row(State(state.clone()),Json(RowChange{
            schema:"public".into(),table:table.clone(),
            original:old,values:Some(new.clone()),delete:false
        })).await else {panic!("stale update must be rejected")};
        assert_eq!(status,StatusCode::CONFLICT);
        let Json(inserted)=insert_row(State(state.clone()),Json(InsertRow{
            schema:"public".into(),table:table.clone(),fields:vec![
                InsertField{column:"Left Key".into(),value:Some("2".into())},
                InsertField{column:"Right Key".into(),value:Some("1".into())},
                InsertField{column:"Payload \"Text\"".into(),value:None},
            ]
        })).await.expect("insert quoted columns");
        assert_eq!(inserted.affected,1);
        change_row(State(state),Json(RowChange{
            schema:"public".into(),table:table.clone(),
            original:new,values:None,delete:true
        })).await.expect("delete one composite key row");
        let count:i64=client.query_one(
            "SELECT count(*) FROM public.\"Datawizz \"\"Pairs\"\"\"",&[]
        ).await.expect("count").get(0);
        assert_eq!(count,2);
        client.batch_execute("DROP TABLE public.\"Datawizz \"\"Pairs\"\"\"")
            .await.expect("cleanup");
    }

    #[tokio::test]
    async fn crud_rejects_unauthorized_columns_and_duplicate_insert_fields() {
        let Some(state)=test_state(200,10_000) else {return;};
        let client=connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_insert_validation (
              id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
              value text NOT NULL DEFAULT 'default'
            )"
        ).await.expect("setup");
        let duplicate=insert_row(State(state.clone()),Json(InsertRow{
            schema:"public".into(),table:"datawizz_insert_validation".into(),
            fields:vec![
                InsertField{column:"value".into(),value:Some("a".into())},
                InsertField{column:"value".into(),value:Some("b".into())}
            ]
        })).await;
        assert!(duplicate.is_err(),"duplicate columns must not be accepted");
        let generated=insert_row(State(state.clone()),Json(InsertRow{
            schema:"public".into(),table:"datawizz_insert_validation".into(),
            fields:vec![InsertField{column:"id".into(),value:Some("100".into())}]
        })).await;
        assert!(generated.is_err(),"identity column must use DEFAULT");
        let not_null=insert_row(State(state),Json(InsertRow{
            schema:"public".into(),table:"datawizz_insert_validation".into(),
            fields:vec![InsertField{column:"value".into(),value:None}]
        })).await;
        assert!(not_null.is_err(),"SQL NULL must not bypass constraints");
        client.batch_execute("DROP TABLE public.datawizz_insert_validation")
            .await.expect("cleanup");
    }

    #[tokio::test]
    async fn insert_uses_defaults_and_preserves_null() {
        let Some(state)=test_state(200,10_000) else {return;};
        let client=connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_insert_test (
              id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
              title text NOT NULL DEFAULT 'default title',
              note text DEFAULT 'default note'
             )"
        ).await.expect("create");
        let insert=|fields| InsertRow{schema:"public".into(),table:"datawizz_insert_test".into(),fields};
        insert_row(State(state.clone()),Json(insert(vec![]))).await.expect("defaults");
        insert_row(State(state.clone()),Json(insert(vec![
            InsertField{column:"title".into(),value:Some("literal NULL".into())},
            InsertField{column:"note".into(),value:None},
        ]))).await.expect("explicit null");
        let rows=client.query("SELECT id,title,note FROM public.datawizz_insert_test ORDER BY id",&[])
            .await.expect("read");
        assert_eq!(rows.len(),2);
        assert_eq!(rows[0].get::<_,String>(1),"default title");
        assert_eq!(rows[0].get::<_,Option<String>>(2),Some("default note".into()));
        assert_eq!(rows[1].get::<_,String>(1),"literal NULL");
        assert_eq!(rows[1].get::<_,Option<String>>(2),None);
        assert!(insert_row(State(state.clone()),Json(insert(vec![
            InsertField{column:"id".into(),value:Some("99".into())}
        ]))).await.is_err());
        assert!(insert_row(State(state),Json(insert(vec![
            InsertField{column:"missing".into(),value:Some("a".into())}
        ]))).await.is_err());
        client.batch_execute("DROP TABLE public.datawizz_insert_test").await.expect("cleanup");
    }

    #[tokio::test]
    async fn changeset_rolls_back_all_rows_on_stale_conflict() {
        let Some(state)=test_state(200,10_000) else {return;};
        let client=connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_changeset_test (id integer PRIMARY KEY, label text);
             INSERT INTO public.datawizz_changeset_test VALUES (1,'first'),(2,'second')"
        ).await.expect("seed");
        let change=|id:&str,old:&str,new:&str|ChangesetItem{
            original:vec![Some(id.into()),Some(old.into())],
            values:vec![Some(id.into()),Some(new.into())],
        };
        let request=|changes|ChangesetRequest{
            schema:"public".into(),table:"datawizz_changeset_test".into(),changes
        };
        let Err((status,_))=apply_changeset(State(state.clone()),Json(request(vec![
            change("1","first","updated"),change("2","stale","changed")
        ]))).await else {panic!("stale row must abort changeset")};
        assert_eq!(status,StatusCode::CONFLICT);
        let rows=client.query(
            "SELECT label FROM public.datawizz_changeset_test ORDER BY id",&[]
        ).await.expect("read after rollback");
        assert_eq!(rows[0].get::<_,String>(0),"first");
        assert_eq!(rows[1].get::<_,String>(0),"second");
        let Json(result)=apply_changeset(State(state),Json(request(vec![
            change("1","first","updated"),change("2","second","changed")
        ]))).await.expect("commit both rows");
        assert_eq!(result.affected,2);
        let rows=client.query(
            "SELECT label FROM public.datawizz_changeset_test ORDER BY id",&[]
        ).await.expect("read after commit");
        assert_eq!(rows[0].get::<_,String>(0),"updated");
        assert_eq!(rows[1].get::<_,String>(0),"changed");
        client.batch_execute("DROP TABLE public.datawizz_changeset_test").await.expect("cleanup");
    }

    #[tokio::test]
    async fn row_edit_respects_pk_null_and_optimistic_conflicts() {
        let Some(state)=test_state(200, 10_000) else {return;};
        let client=connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_row_edit_test (
              id integer PRIMARY KEY, label text, detail text);
             INSERT INTO public.datawizz_row_edit_test VALUES (1, 'before', NULL)"
        ).await.expect("seed");
        let make=|original:Vec<Option<String>>,values:Option<Vec<Option<String>>>,delete| RowChange{
            schema:"public".into(),table:"datawizz_row_edit_test".into(),
            original,values,delete
        };
        let original=vec![Some("1".into()),Some("before".into()),None];
        let updated=vec![Some("1".into()),Some("after".into()),Some("NULL".into())];
        let Json(success)=change_row(State(state.clone()),Json(make(original.clone(),Some(updated.clone()),false)))
            .await.expect("update");
        assert_eq!(success.affected,1);
        let row=client.query_one(
          "SELECT label,detail FROM public.datawizz_row_edit_test WHERE id=1",&[]
        ).await.expect("read");
        assert_eq!(row.get::<_,String>(0),"after");
        assert_eq!(row.get::<_,Option<String>>(1),Some("NULL".into()));
        let Err((code,_))=change_row(State(state.clone()),
            Json(make(original,Some(updated.clone()),false))).await else{panic!("stale update must fail")};
        assert_eq!(code,StatusCode::CONFLICT);
        let Err((code,_))=change_row(State(state.clone()),
            Json(make(updated.clone(),Some(vec![Some("2".into()),Some("after".into()),Some("NULL".into())]),false)))
            .await else{panic!("changing PK must fail")};
        assert_eq!(code,StatusCode::BAD_REQUEST);
        let Json(deleted)=change_row(State(state),Json(make(updated,None,true)))
            .await.expect("delete");
        assert_eq!(deleted.affected,1);
        client.batch_execute("DROP TABLE public.datawizz_row_edit_test")
            .await.expect("cleanup");
    }

    #[tokio::test]
    async fn mutation_endpoints_immediately_visible_on_next_table_fetch() {
        let Some(state) = test_state(200, 10_000) else { return; };
        let client = connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_live_edit_test (id integer PRIMARY KEY, label text);
             INSERT INTO public.datawizz_live_edit_test VALUES (1, 'before')"
        ).await.expect("seed");
        let page = || PageRequest {
            schema:"public".into(),table:"datawizz_live_edit_test".into(),
            offset:Some(0),limit:Some(20)
        };
        let Json(initial)=table_rows(State(state.clone()),Query(page())).await.expect("initial");
        assert_eq!(initial.rows[0][1].as_deref(),Some("before"));
        let original=vec![Some("1".into()),Some("before".into())];
        let changed=vec![Some("1".into()),Some("after".into())];
        change_row(State(state.clone()),Json(RowChange{
            schema:"public".into(),table:"datawizz_live_edit_test".into(),
            original:original.clone(),values:Some(changed.clone()),delete:false
        })).await.expect("update");
        let Json(updated)=table_rows(State(state.clone()),Query(page())).await.expect("refetch update");
        assert_eq!(updated.rows[0][1].as_deref(),Some("after"));
        let Err((code,_))=change_row(State(state.clone()),Json(RowChange{
            schema:"public".into(),table:"datawizz_live_edit_test".into(),
            original:original.clone(),values:Some(changed.clone()),delete:false
        })).await else {panic!("stale update must conflict")};
        assert_eq!(code,StatusCode::CONFLICT);
        let Json(still_updated)=table_rows(State(state.clone()),Query(page())).await.expect("refetch after conflict");
        assert_eq!(still_updated.rows[0][1].as_deref(),Some("after"));
        insert_row(State(state.clone()),Json(InsertRow{
            schema:"public".into(),table:"datawizz_live_edit_test".into(),
            fields:vec![InsertField{column:"id".into(),value:Some("2".into())},
                        InsertField{column:"label".into(),value:Some("inserted".into())}]
        })).await.expect("insert");
        let Json(inserted)=table_rows(State(state.clone()),Query(page())).await.expect("refetch insert");
        assert_eq!(inserted.rows.len(),2);
        assert_eq!(inserted.rows[1][1].as_deref(),Some("inserted"));
        change_row(State(state.clone()),Json(RowChange{
            schema:"public".into(),table:"datawizz_live_edit_test".into(),
            original:changed,values:None,delete:true
        })).await.expect("delete");
        let Json(deleted)=table_rows(State(state.clone()),Query(page())).await.expect("refetch delete");
        assert_eq!(deleted.rows.len(),1);
        assert_eq!(deleted.rows[0][0].as_deref(),Some("2"));
        client.batch_execute("DROP TABLE public.datawizz_live_edit_test").await.expect("cleanup");
    }

    #[tokio::test]
    async fn optimistic_edit_accepts_numeric_scale_and_boolean_wire_formats() {
        let Some(state)=test_state(200,10_000) else {return;};
        let client=connect(&state).await.expect("connect");
        client.batch_execute(
            "CREATE TABLE public.datawizz_typed_edit_test (
                id integer PRIMARY KEY, title text, price numeric(10,2), enabled boolean);
             INSERT INTO public.datawizz_typed_edit_test VALUES (1,'Mechanical keyboard',89.00,true)"
        ).await.expect("seed");
        let req=PageRequest{schema:"public".into(),table:"datawizz_typed_edit_test".into(),offset:Some(0),limit:Some(10)};
        let Json(page)=table_rows(State(state.clone()),Query(req)).await.expect("load row");
        assert_eq!(page.rows[0][2].as_deref(),Some("89.00"));
        assert_eq!(page.rows[0][3].as_deref(),Some("t"));
        let old=page.rows[0].clone();
        let mut changed=old.clone();
        changed[1]=Some("Mechanical keyboard updated".into());
        change_row(State(state.clone()),Json(RowChange{
            schema:"public".into(),table:"datawizz_typed_edit_test".into(),
            original:old.clone(),values:Some(changed.clone()),delete:false
        })).await.expect("typed snapshot update must succeed");
        let row=client.query_one("SELECT title,price,enabled FROM public.datawizz_typed_edit_test WHERE id=1",&[]).await.expect("read");
        assert_eq!(row.get::<_,String>(0),"Mechanical keyboard updated");
        assert!(row.get::<_,bool>(2));
        let Err((status,_))=change_row(State(state.clone()),Json(RowChange{
            schema:"public".into(),table:"datawizz_typed_edit_test".into(),
            original:old,values:Some(changed.clone()),delete:false
        })).await else {panic!("stale typed snapshot must conflict")};
        assert_eq!(status,StatusCode::CONFLICT);
        let mut batch=changed.clone();
        batch[1]=Some("Batch updated".into());
        let Json(saved)=apply_changeset(State(state),Json(ChangesetRequest{
            schema:"public".into(),table:"datawizz_typed_edit_test".into(),
            changes:vec![ChangesetItem{original:changed,values:batch}],
        })).await.expect("typed changeset should succeed");
        assert_eq!(saved.affected,1);
        client.batch_execute("DROP TABLE public.datawizz_typed_edit_test").await.expect("cleanup");
    }

    #[tokio::test]
    async fn table_pagination_reads_multiple_pages() {
        let Some(state) = test_state(200, 10_000) else { return; };
        let client = connect(&state).await.expect("connect");
        // Use a unique schema/object name for deterministic test isolation.
        client.batch_execute(
            "CREATE TABLE IF NOT EXISTS public.datawizz_paging_test (id int PRIMARY KEY);
             TRUNCATE public.datawizz_paging_test;
             INSERT INTO public.datawizz_paging_test SELECT generate_series(1, 205)"
        ).await.expect("seed");
        let make = |offset| PageRequest { schema: "public".into(),
            table: "datawizz_paging_test".into(), offset: Some(offset), limit: Some(100) };
        let Json(first) = table_rows(State(state.clone()), Query(make(0))).await.expect("first");
        let Json(second) = table_rows(State(state.clone()), Query(make(100))).await.expect("second");
        let Json(third) = table_rows(State(state.clone()), Query(make(200))).await.expect("third");
        assert_eq!(first.rows.len(), 100);
        assert_eq!(first.rows[0][0].as_deref(), Some("1"));
        assert!(first.has_next && first.stable_order);
        assert_eq!(second.rows[0][0].as_deref(), Some("101"));
        assert_eq!(third.rows.len(), 5);
        assert!(!third.has_next);
        assert!(table_rows(State(state), Query(make(-1))).await.is_err());
        client.batch_execute("DROP TABLE public.datawizz_paging_test")
            .await.expect("cleanup");
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
    async fn query_accepts_comments_dollar_quotes_and_returning() {
        let Some(state) = test_state(200, 10_000) else {
            return;
        };
        let Json(selected) = execute(
            State(state.clone()),
            Json(QueryRequest {
                sql: "/* comment with ; */ SELECT $tag$semi;colon$tag$ AS value -- trailing ;".into(),
            }),
        )
        .await
        .expect("comments and semicolons in dollar-quoted strings are one statement");
        assert_eq!(selected.rows.len(), 1);
        assert_eq!(selected.rows[0][0].as_deref(), Some("semi;colon"));
        assert!(!selected.truncated);

        let client = connect(&state).await.expect("connect");
        client
            .batch_execute("CREATE TABLE public.datawizz_returning_test (id integer PRIMARY KEY, note text)")
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
        assert_eq!(inserted.rows, vec![vec![Some("1".into()), Some("created".into())]]);
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
