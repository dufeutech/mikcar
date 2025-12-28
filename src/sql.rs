//! SQL database proxy service.
//!
//! Provides a HTTP API for executing SQL queries:
//! - `PostgreSQL`
//! - `MySQL` (future)
//! - `SQLite`
//!
//! ## Script Endpoint
//!
//! The `/script` endpoint allows executing JavaScript code with SQL operations
//! in a single transaction. This enables complex conditional logic:
//!
//! ```javascript
//! // Example: Transfer funds with balance check
//! const from = await sql.query("SELECT balance FROM accounts WHERE id = $1", [fromId]);
//! if (from[0].balance < amount) throw new Error("Insufficient funds");
//! await sql.execute("UPDATE accounts SET balance = balance - $1 WHERE id = $2", [amount, fromId]);
//! await sql.execute("UPDATE accounts SET balance = balance + $1 WHERE id = $2", [amount, toId]);
//! return { success: true, transferred: amount };
//! ```

use crate::{Error, Result, Sidecar};

use axum::{
    extract::State,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, Column, PgPool, Row, Postgres, Transaction};
use std::sync::Arc;
use std::cell::RefCell;
use std::time::Instant;

// =============================================================================
// SQL Audit Logging
// =============================================================================

/// Parsed SQL query information for audit logging.
struct QueryInfo {
    query_type: &'static str,
    table: Option<String>,
    is_dangerous: bool,
}

/// Parse SQL query to extract type and table for audit logging.
fn parse_query_info(sql: &str) -> QueryInfo {
    let sql_upper = sql.trim().to_uppercase();
    let first_word = sql_upper.split_whitespace().next().unwrap_or("");

    let (query_type, is_dangerous) = match first_word {
        "SELECT" => ("SELECT", false),
        "INSERT" => ("INSERT", false),
        "UPDATE" => ("UPDATE", false),
        "DELETE" => ("DELETE", false),
        "CREATE" => ("CREATE", false),
        "ALTER" => ("ALTER", true),
        "DROP" => ("DROP", true),
        "TRUNCATE" => ("TRUNCATE", true),
        "GRANT" => ("GRANT", true),
        "REVOKE" => ("REVOKE", true),
        "BEGIN" | "COMMIT" | "ROLLBACK" => ("TRANSACTION", false),
        _ => ("OTHER", false),
    };

    // Try to extract table name
    let table = extract_table_name(&sql_upper, first_word);

    QueryInfo {
        query_type,
        table,
        is_dangerous,
    }
}

/// Extract table name from SQL query if possible.
fn extract_table_name(sql_upper: &str, query_type: &str) -> Option<String> {
    let words: Vec<&str> = sql_upper.split_whitespace().collect();

    match query_type {
        "SELECT" => {
            // SELECT ... FROM table_name
            words.iter().position(|&w| w == "FROM").and_then(|i| {
                words.get(i + 1).map(|s| s.trim_matches(|c| c == '(' || c == ')').to_lowercase())
            })
        }
        "INSERT" => {
            // INSERT INTO table_name
            words.iter().position(|&w| w == "INTO").and_then(|i| {
                words.get(i + 1).map(|s| s.trim_matches(|c| c == '(' || c == ')').to_lowercase())
            })
        }
        "UPDATE" => {
            // UPDATE table_name
            words.get(1).map(|s| s.trim_matches(|c| c == '(' || c == ')').to_lowercase())
        }
        "DELETE" => {
            // DELETE FROM table_name
            words.iter().position(|&w| w == "FROM").and_then(|i| {
                words.get(i + 1).map(|s| s.trim_matches(|c| c == '(' || c == ')').to_lowercase())
            })
        }
        "DROP" | "TRUNCATE" | "ALTER" => {
            // DROP/TRUNCATE/ALTER TABLE table_name
            words.iter().position(|&w| w == "TABLE").and_then(|i| {
                words.get(i + 1).map(|s| s.trim_matches(|c| c == '(' || c == ')').to_lowercase())
            })
        }
        "CREATE" => {
            // CREATE TABLE table_name
            words.iter().position(|&w| w == "TABLE").and_then(|i| {
                // Skip "IF NOT EXISTS" if present
                let next_idx = if words.get(i + 1) == Some(&"IF") { i + 4 } else { i + 1 };
                words.get(next_idx).map(|s| s.trim_matches(|c| c == '(' || c == ')').to_lowercase())
            })
        }
        _ => None,
    }
}

/// Log a SQL query execution for audit purposes.
fn log_sql_audit(sql: &str, execution_time_ms: u128, rows_affected: Option<u64>, endpoint: &str) {
    let info = parse_query_info(sql);
    let table_str = info.table.as_deref().unwrap_or("unknown");

    if info.is_dangerous {
        tracing::warn!(
            target: "sql_audit",
            query_type = info.query_type,
            table = table_str,
            execution_time_ms = execution_time_ms,
            rows_affected = rows_affected,
            endpoint = endpoint,
            "Dangerous SQL operation executed"
        );
    } else {
        tracing::info!(
            target: "sql_audit",
            query_type = info.query_type,
            table = table_str,
            execution_time_ms = execution_time_ms,
            rows_affected = rows_affected,
            endpoint = endpoint,
            "SQL query executed"
        );
    }
}

/// SQL service configuration.
#[derive(Clone)]
pub struct SqlService {
    pool: PgPool,
}

impl std::fmt::Debug for SqlService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlService")
            .field("pool", &"<PgPool>")
            .finish()
    }
}

impl SqlService {
    /// Create a new SQL service with connection pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create SQL service from environment configuration.
    ///
    /// Reads `DATABASE_URL` environment variable.
    ///
    /// # Errors
    ///
    /// Returns an error if `DATABASE_URL` is not set or the database connection fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::SqlService;
    ///
    /// // Set DATABASE_URL=postgres://user:pass@localhost:5432/mydb
    /// let sql = SqlService::from_env().await?;
    /// ```
    pub async fn from_env() -> Result<Self> {
        let url = std::env::var("DATABASE_URL").map_err(|_| {
            Error::Config("DATABASE_URL environment variable not set".to_string())
        })?;

        Self::from_url(&url).await
    }

    /// Create SQL service from a database URL.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL is invalid or the database connection fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mikcar::SqlService;
    ///
    /// // PostgreSQL connection
    /// let sql = SqlService::from_url("postgres://user:pass@localhost:5432/mydb").await?;
    ///
    /// // With connection options
    /// let sql = SqlService::from_url(
    ///     "postgres://user:pass@localhost:5432/mydb?sslmode=require"
    /// ).await?;
    /// ```
    pub async fn from_url(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(url)
            .await
            .map_err(|e| Error::Config(format!("Failed to connect to database: {e}")))?;

        Ok(Self { pool })
    }
}

impl Sidecar for SqlService {
    fn name(&self) -> &'static str {
        "sql"
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/query", post(execute_query))
            .route("/execute", post(execute_statement))
            .route("/batch", post(execute_batch))
            .route("/script", post(execute_script))
            .with_state(Arc::new(self.pool.clone()))
    }

    fn health_check(&self) -> bool {
        // Verify we can acquire a connection from the pool.
        // Uses block_in_place to avoid blocking the async runtime.
        let pool = self.pool.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        // Try to acquire a connection and run a simple query
                        match sqlx::query("SELECT 1").fetch_one(&pool).await {
                            Ok(_) => true,
                            Err(e) => {
                                tracing::warn!(error = %e, "SQL health check failed");
                                false
                            }
                        }
                    })
                })
            }
            Err(_) => {
                // Not in a tokio context, check if pool is still open
                !pool.is_closed()
            }
        }
    }
}

/// Query request body.
#[derive(Deserialize)]
struct QueryRequest {
    sql: String,
    #[serde(default)]
    params: Vec<serde_json::Value>,
}

/// Query response.
#[derive(Serialize)]
struct QueryResponse {
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
    row_count: usize,
}

/// Execute response.
#[derive(Serialize)]
struct ExecuteResponse {
    rows_affected: u64,
}

/// Batch request body.
#[derive(Deserialize)]
struct BatchRequest {
    statements: Vec<QueryRequest>,
}

/// Script request body.
#[derive(Deserialize)]
struct ScriptRequest {
    /// JavaScript code to execute.
    /// Has access to `sql.query(sql, params)` and `sql.execute(sql, params)`.
    /// Use `return` to specify the response value.
    script: String,
    /// Input variables accessible in the script via `input` object.
    #[serde(default)]
    input: serde_json::Value,
}

/// Script response.
#[derive(Serialize)]
struct ScriptResponse {
    /// Result returned from the script (via `return` statement).
    result: serde_json::Value,
    /// Number of queries executed.
    queries_executed: usize,
}

/// Execute a SELECT query.
async fn execute_query(
    State(pool): State<Arc<PgPool>>,
    Json(body): Json<QueryRequest>,
) -> Result<impl IntoResponse> {
    let start = Instant::now();
    let mut query = sqlx::query(&body.sql);

    // Bind parameters
    for param in &body.params {
        query = bind_param(query, param);
    }

    let rows = query.fetch_all(pool.as_ref()).await?;
    let execution_time_ms = start.elapsed().as_millis();

    // Convert rows to JSON
    let json_rows: Vec<serde_json::Map<String, serde_json::Value>> = rows
        .iter()
        .map(|row| {
            let mut map = serde_json::Map::new();
            for (i, column) in row.columns().iter().enumerate() {
                let value = row_to_json_value(row, i);
                map.insert(column.name().to_string(), value);
            }
            map
        })
        .collect();

    // Audit log
    log_sql_audit(&body.sql, execution_time_ms, Some(json_rows.len() as u64), "/query");

    let response = QueryResponse {
        row_count: json_rows.len(),
        rows: json_rows,
    };

    Ok(Json(response))
}

/// Execute an INSERT/UPDATE/DELETE statement.
async fn execute_statement(
    State(pool): State<Arc<PgPool>>,
    Json(body): Json<QueryRequest>,
) -> Result<impl IntoResponse> {
    let start = Instant::now();
    let mut query = sqlx::query(&body.sql);

    // Bind parameters
    for param in &body.params {
        query = bind_param(query, param);
    }

    let result = query.execute(pool.as_ref()).await?;
    let execution_time_ms = start.elapsed().as_millis();
    let rows_affected = result.rows_affected();

    // Audit log
    log_sql_audit(&body.sql, execution_time_ms, Some(rows_affected), "/execute");

    Ok(Json(ExecuteResponse { rows_affected }))
}

/// Execute multiple statements in a transaction.
async fn execute_batch(
    State(pool): State<Arc<PgPool>>,
    Json(body): Json<BatchRequest>,
) -> Result<impl IntoResponse> {
    let batch_start = Instant::now();
    let mut tx = pool.begin().await?;
    let mut total_affected = 0u64;

    for stmt in &body.statements {
        let stmt_start = Instant::now();
        let mut query = sqlx::query(&stmt.sql);

        for param in &stmt.params {
            query = bind_param(query, param);
        }

        let result = query.execute(&mut *tx).await?;
        let rows_affected = result.rows_affected();
        total_affected += rows_affected;

        // Audit log each statement in batch
        log_sql_audit(&stmt.sql, stmt_start.elapsed().as_millis(), Some(rows_affected), "/batch");
    }

    tx.commit().await?;

    tracing::info!(
        target: "sql_audit",
        statements_executed = body.statements.len(),
        total_rows_affected = total_affected,
        total_execution_time_ms = batch_start.elapsed().as_millis(),
        endpoint = "/batch",
        "Batch transaction committed"
    );

    Ok(Json(serde_json::json!({
        "status": "ok",
        "statements_executed": body.statements.len(),
        "total_rows_affected": total_affected
    })))
}

/// Bind a JSON parameter to a query.
fn bind_param<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    param: &'q serde_json::Value,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match param {
        serde_json::Value::Null => query.bind(Option::<String>::None),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(n.to_string())
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        // For arrays and objects, bind as JSON
        v => query.bind(v.clone()),
    }
}

/// Convert a row column to JSON value.
fn row_to_json_value(row: &sqlx::postgres::PgRow, index: usize) -> serde_json::Value {
    // Try different types in order of likelihood
    // Note: Order matters - try more specific types first

    // Integer types
    if let Ok(v) = row.try_get::<i64, _>(index) {
        return serde_json::Value::Number(v.into());
    }
    if let Ok(v) = row.try_get::<i32, _>(index) {
        return serde_json::Value::Number(v.into());
    }

    // Float types (includes REAL, DOUBLE PRECISION)
    if let Ok(v) = row.try_get::<f64, _>(index) {
        return serde_json::Number::from_f64(v)
            .map_or(serde_json::Value::Null, serde_json::Value::Number);
    }
    if let Ok(v) = row.try_get::<f32, _>(index) {
        return serde_json::Number::from_f64(f64::from(v))
            .map_or(serde_json::Value::Null, serde_json::Value::Number);
    }

    // DECIMAL/NUMERIC - keep as string to preserve precision
    // Converting to float would lose precision for financial values
    if let Ok(v) = row.try_get::<sqlx::types::BigDecimal, _>(index) {
        return serde_json::Value::String(v.to_string());
    }

    // Boolean
    if let Ok(v) = row.try_get::<bool, _>(index) {
        return serde_json::Value::Bool(v);
    }

    // String types (TEXT, VARCHAR, etc.)
    if let Ok(v) = row.try_get::<String, _>(index) {
        return serde_json::Value::String(v);
    }

    // JSON/JSONB
    if let Ok(v) = row.try_get::<serde_json::Value, _>(index) {
        return v;
    }

    // UUID
    if let Ok(v) = row.try_get::<uuid::Uuid, _>(index) {
        return serde_json::Value::String(v.to_string());
    }

    // Timestamps
    if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(index) {
        return serde_json::Value::String(v.to_string());
    }
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(index) {
        return serde_json::Value::String(v.to_rfc3339());
    }

    serde_json::Value::Null
}

// =============================================================================
// JavaScript Script Execution
// =============================================================================

use rquickjs::{Context, Runtime, Function, Object, Value as JsValue, FromJs};

/// Message sent from JS to Rust for SQL execution.
#[derive(Debug)]
enum SqlMessage {
    Query {
        sql: String,
        params: Vec<serde_json::Value>,
        response_tx: std::sync::mpsc::Sender<std::result::Result<Vec<serde_json::Map<String, serde_json::Value>>, String>>,
    },
    Execute {
        sql: String,
        params: Vec<serde_json::Value>,
        response_tx: std::sync::mpsc::Sender<std::result::Result<u64, String>>,
    },
}

/// Thread-safe state for SQL operations from JS.
struct SqlBridge {
    tx: tokio::sync::mpsc::UnboundedSender<SqlMessage>,
    query_count: std::sync::atomic::AtomicUsize,
}

// Global state for current script execution (thread-local)
thread_local! {
    static SQL_BRIDGE: RefCell<Option<Arc<SqlBridge>>> = const { RefCell::new(None) };
}

/// Execute JavaScript code with SQL operations in a transaction.
///
/// The script has access to:
/// - `sql.query(sql, params)` - Execute a SELECT query, returns array of row objects
/// - `sql.execute(sql, params)` - Execute INSERT/UPDATE/DELETE, returns rows affected
/// - `input` - The input object passed in the request
///
/// All operations run within a single database transaction.
/// If the script throws an error or any SQL fails, the transaction is rolled back.
async fn execute_script(
    State(pool): State<Arc<PgPool>>,
    Json(body): Json<ScriptRequest>,
) -> Result<impl IntoResponse> {
    // Start transaction FIRST - this is critical for ACID compliance
    let mut tx = pool.begin().await?;

    // Channel for SQL operations
    let (sql_tx, mut sql_rx) = tokio::sync::mpsc::unbounded_channel::<SqlMessage>();

    // Counter for executed queries
    let bridge = Arc::new(SqlBridge {
        tx: sql_tx,
        query_count: std::sync::atomic::AtomicUsize::new(0),
    });
    let bridge_clone = bridge.clone();

    // Preprocess the script (supports export default or legacy IIFE)
    let wrapped_script = preprocess_script(&body.script);

    let input_json = body.input.clone();
    let script = wrapped_script.clone();

    // Spawn the JS execution in a blocking task
    let mut js_handle = tokio::task::spawn_blocking(move || {
        run_js_script(&script, &input_json, bridge_clone)
    });

    // Process SQL messages from JS while it runs
    let mut last_error: Option<String> = None;

    loop {
        tokio::select! {
            // Check if JS finished
            js_result = &mut js_handle => {
                match js_result {
                    Ok(Ok(result)) => {
                        if last_error.is_some() {
                            // SQL error occurred, rollback
                            tx.rollback().await?;
                            return Err(Error::Internal(last_error.unwrap()));
                        }
                        // Success - commit transaction
                        tx.commit().await?;
                        return Ok(Json(ScriptResponse {
                            result,
                            queries_executed: bridge.query_count.load(std::sync::atomic::Ordering::Relaxed),
                        }));
                    }
                    Ok(Err(e)) => {
                        // JS error - rollback
                        tx.rollback().await?;
                        return Err(Error::Internal(e));
                    }
                    Err(e) => {
                        // Task panic - rollback
                        tx.rollback().await?;
                        return Err(Error::Internal(format!("Script execution panicked: {e}")));
                    }
                }
            }

            // Process SQL messages
            msg = sql_rx.recv() => {
                match msg {
                    Some(SqlMessage::Query { sql, params, response_tx }) => {
                        let result = execute_query_in_tx(&mut tx, &sql, &params).await;
                        match result {
                            Ok(rows) => {
                                let _ = response_tx.send(Ok(rows));
                            }
                            Err(e) => {
                                last_error = Some(e.to_string());
                                let _ = response_tx.send(Err(e.to_string()));
                            }
                        }
                    }
                    Some(SqlMessage::Execute { sql, params, response_tx }) => {
                        let result = execute_statement_in_tx(&mut tx, &sql, &params).await;
                        match result {
                            Ok(affected) => {
                                let _ = response_tx.send(Ok(affected));
                            }
                            Err(e) => {
                                last_error = Some(e.to_string());
                                let _ = response_tx.send(Err(e.to_string()));
                            }
                        }
                    }
                    None => {
                        // Channel closed, JS finished
                        break;
                    }
                }
            }
        }
    }

    // If we get here, wait for JS to finish
    match js_handle.await {
        Ok(Ok(result)) => {
            if let Some(err) = last_error {
                tx.rollback().await?;
                return Err(Error::Internal(err));
            }
            tx.commit().await?;
            Ok(Json(ScriptResponse {
                result,
                queries_executed: bridge.query_count.load(std::sync::atomic::Ordering::Relaxed),
            }))
        }
        Ok(Err(e)) => {
            tx.rollback().await?;
            Err(Error::Internal(e))
        }
        Err(e) => {
            tx.rollback().await?;
            Err(Error::Internal(format!("Script execution panicked: {e}")))
        }
    }
}

/// Perform a SQL query using the thread-local bridge.
fn bridge_query(sql: String, params: Vec<serde_json::Value>) -> std::result::Result<Vec<serde_json::Map<String, serde_json::Value>>, String> {
    SQL_BRIDGE.with(|cell| {
        let bridge = cell.borrow();
        let bridge = bridge.as_ref().ok_or("SQL bridge not initialized")?;

        let (resp_tx, resp_rx) = std::sync::mpsc::channel();
        bridge.tx.send(SqlMessage::Query {
            sql,
            params,
            response_tx: resp_tx,
        }).map_err(|e| e.to_string())?;

        bridge.query_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        resp_rx.recv().map_err(|e| e.to_string())?
    })
}

/// Perform a SQL execute using the thread-local bridge.
fn bridge_execute(sql: String, params: Vec<serde_json::Value>) -> std::result::Result<u64, String> {
    SQL_BRIDGE.with(|cell| {
        let bridge = cell.borrow();
        let bridge = bridge.as_ref().ok_or("SQL bridge not initialized")?;

        let (resp_tx, resp_rx) = std::sync::mpsc::channel();
        bridge.tx.send(SqlMessage::Execute {
            sql,
            params,
            response_tx: resp_tx,
        }).map_err(|e| e.to_string())?;

        bridge.query_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        resp_rx.recv().map_err(|e| e.to_string())?
    })
}

/// Run JavaScript with sql object.
fn run_js_script(
    script: &str,
    input: &serde_json::Value,
    bridge: Arc<SqlBridge>,
) -> std::result::Result<serde_json::Value, String> {
    // Set thread-local bridge
    SQL_BRIDGE.with(|cell| {
        *cell.borrow_mut() = Some(bridge);
    });

    // Create QuickJS runtime
    let runtime = Runtime::new().map_err(|e| format!("Failed to create JS runtime: {e}"))?;
    let context = Context::full(&runtime).map_err(|e| format!("Failed to create JS context: {e}"))?;

    let result = context.with(|ctx| {
        let globals = ctx.globals();

        // Register native functions that use JSON strings to avoid lifetime issues
        // __sql_query(sql, params_json) -> rows_json
        let query_fn = Function::new(ctx.clone(), native_query)
            .map_err(|e| format!("Failed to create query function: {e}"))?;

        // __sql_execute(sql, params_json) -> rows_affected
        let execute_fn = Function::new(ctx.clone(), native_execute)
            .map_err(|e| format!("Failed to create execute function: {e}"))?;

        globals.set("__sql_query", query_fn).map_err(|e| format!("Failed to set __sql_query: {e}"))?;
        globals.set("__sql_execute", execute_fn).map_err(|e| format!("Failed to set __sql_execute: {e}"))?;

        // Create sql wrapper in JavaScript that provides a nice API
        let sql_wrapper = r"
            var sql = {
                query: function(sqlStr, params) {
                    var result = __sql_query(sqlStr, JSON.stringify(params || []));
                    return JSON.parse(result);
                },
                execute: function(sqlStr, params) {
                    return __sql_execute(sqlStr, JSON.stringify(params || []));
                }
            };
        ";
        ctx.eval::<(), _>(sql_wrapper).map_err(|e| format!("Failed to create sql wrapper: {e}"))?;

        // Set input object
        let input_json = serde_json::to_string(&input).map_err(|e| format!("Failed to serialize input: {e}"))?;
        let input_script = format!("var input = {input_json};");
        ctx.eval::<(), _>(input_script.as_str()).map_err(|e| format!("Failed to set input: {e}"))?;

        // Execute the script
        let result: JsValue<'_> = ctx.eval(script).map_err(|e| format!("Script error: {e}"))?;

        // Convert result to JSON
        js_to_json(&ctx, result)
    });

    // Clear thread-local bridge
    SQL_BRIDGE.with(|cell| {
        *cell.borrow_mut() = None;
    });

    result
}

/// Native query function - takes SQL and params as JSON string, returns JSON string
#[allow(clippy::needless_pass_by_value)] // Required by rquickjs FFI
fn native_query(sql: String, params_json: String) -> rquickjs::Result<String> {
    let params: Vec<serde_json::Value> = serde_json::from_str(&params_json)
        .map_err(|_| rquickjs::Error::Exception)?;

    match bridge_query(sql, params) {
        Ok(rows) => {
            serde_json::to_string(&rows).map_err(|_| rquickjs::Error::Exception)
        }
        Err(_) => Err(rquickjs::Error::Exception)
    }
}

/// Native execute function - takes SQL and params as JSON string, returns rows affected
#[allow(clippy::needless_pass_by_value)] // Required by rquickjs FFI
fn native_execute(sql: String, params_json: String) -> rquickjs::Result<i64> {
    let params: Vec<serde_json::Value> = serde_json::from_str(&params_json)
        .map_err(|_| rquickjs::Error::Exception)?;

    match bridge_execute(sql, params) {
        #[allow(clippy::cast_possible_wrap)] // Row count won't exceed i64::MAX
        Ok(affected) => Ok(affected as i64),
        Err(_) => Err(rquickjs::Error::Exception)
    }
}

/// Execute a query within a transaction.
async fn execute_query_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    sql: &str,
    params: &[serde_json::Value],
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let start = Instant::now();
    let mut query = sqlx::query(sql);

    for param in params {
        query = bind_param(query, param);
    }

    let rows = query.fetch_all(&mut **tx).await?;
    let execution_time_ms = start.elapsed().as_millis();

    let json_rows: Vec<serde_json::Map<String, serde_json::Value>> = rows
        .iter()
        .map(|row| {
            let mut map = serde_json::Map::new();
            for (i, column) in row.columns().iter().enumerate() {
                let value = row_to_json_value(row, i);
                map.insert(column.name().to_string(), value);
            }
            map
        })
        .collect();

    // Audit log
    log_sql_audit(sql, execution_time_ms, Some(json_rows.len() as u64), "/script");

    Ok(json_rows)
}

/// Execute a statement within a transaction.
async fn execute_statement_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    sql: &str,
    params: &[serde_json::Value],
) -> Result<u64> {
    let start = Instant::now();
    let mut query = sqlx::query(sql);

    for param in params {
        query = bind_param(query, param);
    }

    let result = query.execute(&mut **tx).await?;
    let execution_time_ms = start.elapsed().as_millis();
    let rows_affected = result.rows_affected();

    // Audit log
    log_sql_audit(sql, execution_time_ms, Some(rows_affected), "/script");

    Ok(rows_affected)
}

// =============================================================================
// JSON <-> JavaScript Value Conversion
// =============================================================================

#[allow(clippy::needless_pass_by_value)] // Value is cloned internally for type checking
fn js_to_json<'js>(ctx: &rquickjs::Ctx<'js>, value: JsValue<'js>) -> std::result::Result<serde_json::Value, String> {
    if value.is_null() || value.is_undefined() {
        Ok(serde_json::Value::Null)
    } else if let Some(b) = value.as_bool() {
        Ok(serde_json::Value::Bool(b))
    } else if let Some(i) = value.as_int() {
        Ok(serde_json::Value::Number(i.into()))
    } else if let Some(f) = value.as_float() {
        Ok(serde_json::Number::from_f64(f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number))
    } else if let Ok(s) = String::from_js(ctx, value.clone()) {
        Ok(serde_json::Value::String(s))
    } else if let Ok(arr) = rquickjs::Array::from_js(ctx, value.clone()) {
        let mut json_arr = Vec::new();
        for i in 0..arr.len() {
            if let Ok(item) = arr.get::<JsValue<'_>>(i) {
                json_arr.push(js_to_json(ctx, item)?);
            }
        }
        Ok(serde_json::Value::Array(json_arr))
    } else if let Ok(obj) = Object::from_js(ctx, value.clone()) {
        let mut json_obj = serde_json::Map::new();
        for (key, val) in obj.props::<String, JsValue<'_>>().flatten() {
            json_obj.insert(key, js_to_json(ctx, val)?);
        }
        Ok(serde_json::Value::Object(json_obj))
    } else {
        Ok(serde_json::Value::Null)
    }
}

// =============================================================================
// Script Preprocessing
// =============================================================================

/// Preprocess a script to support multiple formats.
///
/// Supported formats:
///
/// 1. Export default function (recommended):
/// ```js
/// export default function(input) { return input.value; }
/// ```
///
/// 2. Raw code with return (simple scripts):
/// ```js
/// var result = sql.query("SELECT * FROM users");
/// return { count: result.length };
/// ```
///
/// 3. Raw expression (simplest):
/// ```js
/// sql.query("SELECT 1")
/// ```
///
/// All formats are wrapped appropriately and called with the `input` object.
fn preprocess_script(script: &str) -> String {
    let trimmed = script.trim();

    // Check if it's an export default function
    if trimmed.starts_with("export default") {
        // Replace "export default" with variable assignment
        let transformed = script
            .replace("export default async function", "var __default__ = async function")
            .replace("export default function", "var __default__ = function")
            .replace("export default async (", "var __default__ = async function(")
            .replace("export default (", "var __default__ = function(");

        // Call the default export with input
        format!("{transformed}\n__default__(input);")
    } else {
        // Wrap raw code in a function
        // This handles scripts like:
        //   var x = sql.query(...); return {result: x};
        // or just:
        //   sql.query("SELECT 1")
        format!("(function(input) {{\n{script}\n}})(input);")
    }
}
