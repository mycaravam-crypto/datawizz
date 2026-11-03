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
use serde::{Deserialize, Serialize};
use tokio_postgres::{NoTls, SimpleQueryMessage};
use native_tls::{Certificate, TlsConnector};
use postgres_native_tls::MakeTlsConnector;
use subtle::ConstantTimeEq;

#[derive(Clone)]
struct AppState {
    url: Arc<String>,
    max_rows: usize,
    timeout_ms: u64,
    tls: Option<Arc<TlsConnector>>,
    access_token: Option<Arc<String>>,
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

// Keep database credentials and TLS diagnostics out of HTTP responses and logs.
fn connection_error() -> (StatusCode, Json<ApiError>) {
    api_error(StatusCode::BAD_GATEWAY, "PostgreSQL connection failed")
}

async fn connect(state: &AppState) -> Result<tokio_postgres::Client, (StatusCode, Json<ApiError>)> {
    // Per-request PostgreSQL connection; use verified TLS when explicitly configured.
    let client = if let Some(tls) = &state.tls {
        let (client, connection) = tokio_postgres::connect(
            &state.url, MakeTlsConnector::new((**tls).clone())
        ).await.map_err(|_| connection_error())?;
        tokio::spawn(async move { let _ = connection.await; });
        client
    } else {
        let (client, connection) = tokio_postgres::connect(&state.url, NoTls)
            .await.map_err(|_| connection_error())?;
        tokio::spawn(async move { let _ = connection.await; });
        client
    };
    client.batch_execute(&format!("SET statement_timeout = {}", state.timeout_ms))
        .await.map_err(|_| api_error(StatusCode::BAD_GATEWAY, "Could not configure query timeout"))?;
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
    State(state): State<AppState>, request: Request, next: Next,
) -> impl IntoResponse {
    let token = request.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !authorized(state.access_token.as_deref().map(String::as_str), token) {
        return api_error(StatusCode::UNAUTHORIZED, "Authentication required").into_response();
    }
    next.run(request).await
}

fn verified_pg_tls(mode: &str, ca_path: Option<&str>) -> Result<Option<Arc<TlsConnector>>, String> {
    match mode {
        "disable" => {
            if ca_path.is_some() { return Err("DATABASE_TLS_CA_FILE requires DATABASE_TLS=verify-full".into()); }
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
            let connector = builder.build().map_err(|_| "Cannot initialize TLS trust store")?;
            Ok(Some(Arc::new(connector)))
        }
        _ => Err("DATABASE_TLS must be 'disable' or 'verify-full'".into()),
    }
}

#[derive(Serialize)]
struct ConnectionOption { id: String, label: String, engine: String }

// The first increment exposes only the configured default connection, never its DSN.
async fn connections() -> Json<Vec<ConnectionOption>> {
    Json(vec![ConnectionOption {
        id: "default".into(),
        label: "Default PostgreSQL".into(),
        engine: "postgresql".into(),
    }])
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
}

#[derive(Serialize)]
struct AdminIndex {
    schema: String,
    table: String,
    name: String,
    valid: bool,
    scans: i64,
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
    let databases = client.query(
        "SELECT datname::text, pg_catalog.pg_get_userbyid(datdba)::text, datallowconn
         FROM pg_catalog.pg_database ORDER BY datname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let roles = client.query(
        "SELECT rolname::text, rolcanlogin, rolsuper, rolcreatedb, rolcreaterole
         FROM pg_catalog.pg_roles ORDER BY rolname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let memberships = client.query(
        "SELECT r.rolname::text, m.rolname::text, am.admin_option
         FROM pg_catalog.pg_auth_members am
         JOIN pg_catalog.pg_roles r ON r.oid = am.roleid
         JOIN pg_catalog.pg_roles m ON m.oid = am.member
         ORDER BY r.rolname, m.rolname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let sessions = client.query(
        "SELECT pid, datname::text, usename::text, state::text, wait_event_type::text
         FROM pg_catalog.pg_stat_activity ORDER BY pid LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let extensions = client.query(
        "SELECT e.extname::text, e.extversion::text, n.nspname::text
         FROM pg_catalog.pg_extension e
         JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
         ORDER BY e.extname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;


    let schemas = client.query(
        "SELECT n.nspname::text, pg_catalog.pg_get_userbyid(n.nspowner)::text,
                has_schema_privilege(n.oid, 'USAGE'), has_schema_privilege(n.oid, 'CREATE')
         FROM pg_catalog.pg_namespace n
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let relations = client.query(
        "SELECT n.nspname::text, c.relname::text,
                CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned table'
                     WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view'
                     WHEN 'f' THEN 'foreign table' WHEN 'S' THEN 'sequence' END::text,
                pg_catalog.pg_get_userbyid(c.relowner)::text,
                GREATEST(c.reltuples::bigint, 0),
                CASE WHEN c.relkind IN ('r','p','m','S') THEN pg_catalog.pg_total_relation_size(c.oid)
                     ELSE 0::bigint END
         FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('r','p','v','m','f','S')
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname, c.relname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let indexes = client.query(
        "SELECT s.schemaname::text, s.relname::text, s.indexrelname::text,
                i.indisvalid, COALESCE(s.idx_scan, 0)::bigint
         FROM pg_catalog.pg_stat_user_indexes s
         JOIN pg_catalog.pg_index i ON i.indexrelid = s.indexrelid
         ORDER BY s.schemaname, s.relname, s.indexrelname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let constraints = client.query(
        "SELECT n.nspname::text, c.relname::text, con.conname::text,
                con.contype::text, pg_catalog.pg_get_constraintdef(con.oid)::text
         FROM pg_catalog.pg_constraint con
         JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname, c.relname, con.conname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let table_stats = client.query(
        "SELECT s.schemaname::text, s.relname::text, s.n_live_tup::bigint,
                s.n_dead_tup::bigint, s.seq_scan::bigint, COALESCE(s.idx_scan, 0)::bigint,
                to_char(s.last_autovacuum, 'YYYY-MM-DD HH24:MI')::text,
                to_char(s.last_autoanalyze, 'YYYY-MM-DD HH24:MI')::text
         FROM pg_catalog.pg_stat_user_tables s
         ORDER BY s.schemaname, s.relname LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let locks = client.query(
        "SELECT l.pid,
                (n.nspname || '.' || c.relname)::text,
                l.mode::text, l.granted
         FROM pg_catalog.pg_locks l
         JOIN pg_catalog.pg_class c ON c.oid = l.relation
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE l.locktype = 'relation'
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY l.granted, l.pid NULLS LAST LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    // Explicit allowlist: pg_settings may contain infrastructure paths and sensitive values.
    let settings = client.query(
        "SELECT name::text, setting::text, unit::text, pending_restart
         FROM pg_catalog.pg_settings
         WHERE name IN ('server_version', 'max_connections', 'shared_buffers', 'work_mem',
                        'maintenance_work_mem', 'statement_timeout', 'autovacuum',
                        'max_wal_size', 'wal_level', 'idle_in_transaction_session_timeout')
         ORDER BY name LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let privileges = client.query(
        "SELECT table_schema::text, table_name::text, grantee::text,
                privilege_type::text, (is_grantable = 'YES')
         FROM information_schema.role_table_grants
         ORDER BY table_schema, table_name, grantee, privilege_type LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;
    let routines = client.query(
        "SELECT n.nspname::text, p.proname::text,
                pg_catalog.pg_get_function_identity_arguments(p.oid)::text,
                CASE WHEN p.prokind = 'p' THEN 'procedure' ELSE 'function' END::text,
                pg_catalog.pg_get_userbyid(p.proowner)::text
         FROM pg_catalog.pg_proc p
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg_toast%' AND n.nspname NOT LIKE 'pg_temp%'
         ORDER BY n.nspname, p.proname, p.oid LIMIT 100", &[]
    ).await.map_err(|e| postgres_error(&e))?;

    Ok(Json(AdminOverview {
        databases: databases.iter().map(|r| AdminDatabase {
            name: r.get(0), owner: r.get(1), allow_connections: r.get(2),
        }).collect(),
        roles: roles.iter().map(|r| AdminRole {
            name: r.get(0), can_login: r.get(1), superuser: r.get(2),
            create_db: r.get(3), create_role: r.get(4),
        }).collect(),
        memberships: memberships.iter().map(|r| AdminMembership {
            role: r.get(0), member: r.get(1), admin_option: r.get(2),
        }).collect(),
        sessions: sessions.iter().map(|r| AdminSession {
            pid: r.get(0), database: r.get(1), username: r.get(2),
            state: r.get(3), wait_event_type: r.get(4),
        }).collect(),
        extensions: extensions.iter().map(|r| AdminExtension {
            name: r.get(0), version: r.get(1), schema: r.get(2),
        }).collect(),

        schemas: schemas.iter().map(|r| AdminSchema {
            name: r.get(0), owner: r.get(1), can_use: r.get(2), can_create: r.get(3),
        }).collect(),
        relations: relations.iter().map(|r| AdminRelation {
            schema: r.get(0), name: r.get(1), kind: r.get(2), owner: r.get(3),
            estimated_rows: r.get(4), size_bytes: r.get(5),
        }).collect(),
        indexes: indexes.iter().map(|r| AdminIndex {
            schema: r.get(0), table: r.get(1), name: r.get(2),
            valid: r.get(3), scans: r.get(4),
        }).collect(),
        constraints: constraints.iter().map(|r| AdminConstraint {
            schema: r.get(0), table: r.get(1), name: r.get(2),
            kind: r.get(3), definition: r.get(4),
        }).collect(),
        table_stats: table_stats.iter().map(|r| AdminTableStats {
            schema: r.get(0), table: r.get(1), live_rows: r.get(2),
            dead_rows: r.get(3), seq_scans: r.get(4), index_scans: r.get(5),
            last_autovacuum: r.get(6), last_autoanalyze: r.get(7),
        }).collect(),
        locks: locks.iter().map(|r| AdminLock {
            pid: r.get(0), relation: r.get(1), mode: r.get(2), granted: r.get(3),
        }).collect(),
        settings: settings.iter().map(|r| AdminSetting {
            name: r.get(0), value: r.get(1), unit: r.get(2), pending_restart: r.get(3),
        }).collect(),
        privileges: privileges.iter().map(|r| AdminPrivilege {
            schema: r.get(0), table: r.get(1), grantee: r.get(2),
            privilege: r.get(3), grantable: r.get(4),
        }).collect(),
        routines: routines.iter().map(|r| AdminRoutine {
            schema: r.get(0), name: r.get(1), arguments: r.get(2),
            kind: r.get(3), owner: r.get(4),
        }).collect(),
    }))
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
    let tls_mode = env::var("DATABASE_TLS").unwrap_or_else(|_| "disable".into());
    let tls = verified_pg_tls(&tls_mode, env::var("DATABASE_TLS_CA_FILE").ok().as_deref())
        .expect("Invalid PostgreSQL TLS configuration");
    let access_token = env::var("DATAWIZZ_ACCESS_TOKEN").ok().map(|value| {
        assert!(value.len() >= 32 && value.len() <= 4096,
            "DATAWIZZ_ACCESS_TOKEN must contain 32..4096 characters");
        Arc::new(value)
    });
    let state = AppState { url: Arc::new(url), max_rows, timeout_ms, tls, access_token };
    // Fail fast before accepting browser traffic.
    connect(&state).await.expect("Cannot connect to PostgreSQL");
    let api = Router::new()
        .route("/api/connections", get(connections))
        .route("/api/schema", get(schema))
        .route("/api/admin", get(admin_overview))
        .route("/api/columns", get(columns))
        .route("/api/query", post(execute))
        .route("/api/table-rows", get(table_rows))
        .route("/api/row-change", post(change_row))
        .route("/api/changeset", post(apply_changeset))
        .route("/api/row-insert", post(insert_row))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_access_token));
    let app = Router::new()
        .route("/", get(index))
        .merge(api)
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

    #[test]
    fn access_token_is_opt_in_and_uses_exact_bearer_header() {
        let secret = "0123456789abcdefghijklmnopqrstuvwxyz";
        assert!(authorized(None, None));
        assert!(!authorized(Some(secret), None));
        assert!(!authorized(Some(secret), Some("Bearer incorrect")));
        assert!(!authorized(Some(secret), Some("Basic other")));
        assert!(authorized(Some(secret), Some("Bearer 0123456789abcdefghijklmnopqrstuvwxyz")));
    }

    #[test]
    fn tls_requires_verified_configuration() {
        assert!(verified_pg_tls("disable", None).unwrap().is_none());
        assert!(verified_pg_tls("disable", Some("unused")).is_err());
        assert!(verified_pg_tls("insecure", None).is_err());
        assert!(verified_pg_tls("verify-full", Some("/missing/datawizz/ca.pem")).is_err());
        assert!(verified_pg_tls("verify-full", None).unwrap().is_some());
    }

    #[tokio::test]
    async fn connection_options_do_not_expose_credentials() {
        let Json(profiles) = connections().await;
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "default");
        let body = serde_json::to_string(&profiles).expect("serialize options");
        assert!(!body.contains("postgres://"));
        assert!(!body.contains("password"));
    }

    #[tokio::test]
    async fn admin_inventory_is_read_only_and_omits_sensitive_fields() {
        let Some(state) = test_state(200, 10_000) else { return; };
        let Json(overview) = admin_overview(State(state)).await.expect("read admin catalog");
        assert!(!overview.databases.is_empty());
        assert!(!overview.roles.is_empty());
        assert!(!overview.sessions.is_empty());
        assert!(!overview.extensions.is_empty());
        let json = serde_json::to_string(&overview).expect("serialize admin overview");
        for hidden in ["rolpassword", "client_addr", "\"query\"", "DATABASE_URL"] {
            assert!(!json.contains(hidden), "admin response must not expose {hidden}");
        }
    }


    #[tokio::test]
    async fn admin_details_cover_schemas_constraints_indexes_and_safe_settings() {
        let Some(state) = test_state(200, 10_000) else { return; };
        let client = connect(&state).await.expect("connect");
        client.batch_execute(
            "DROP SCHEMA IF EXISTS datawizz_admin_test CASCADE;
             CREATE SCHEMA datawizz_admin_test;
             CREATE TABLE datawizz_admin_test.items (id integer PRIMARY KEY, label text);
             CREATE INDEX items_label_idx ON datawizz_admin_test.items (label);"
        ).await.expect("create admin catalog fixtures");

        let Json(admin) = admin_overview(State(state)).await.expect("read admin details");
        assert!(admin.schemas.iter().any(|s| s.name == "datawizz_admin_test"));
        assert!(admin.relations.iter().any(|r| r.schema == "datawizz_admin_test"
            && r.name == "items" && r.kind == "table"));
        assert!(admin.indexes.iter().any(|i| i.schema == "datawizz_admin_test"
            && i.name == "items_label_idx" && i.valid));
        assert!(admin.constraints.iter().any(|c| c.schema == "datawizz_admin_test"
            && c.name == "items_pkey" && c.kind == "p"));
        assert!(admin.table_stats.iter().any(|t| t.schema == "datawizz_admin_test" && t.table == "items"));
        assert!(admin.settings.iter().any(|s| s.name == "max_connections"));
        assert!(admin.settings.iter().all(|s| matches!(s.name.as_str(),
            "server_version" | "max_connections" | "shared_buffers" | "work_mem"
            | "maintenance_work_mem" | "statement_timeout" | "autovacuum"
            | "max_wal_size" | "wal_level" | "idle_in_transaction_session_timeout")));
        let serialized = serde_json::to_string(&admin).expect("serialize inventory");
        for secret in ["rolpassword", "client_addr", "\"query\"", "ssl_key_file"] {
            assert!(!serialized.contains(secret), "unexpected sensitive admin field: {secret}");
        }
        client.batch_execute("DROP SCHEMA datawizz_admin_test CASCADE")
            .await.expect("cleanup admin fixtures");
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
