use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Error as IoError, ErrorKind};
use std::num::NonZero;
use std::path::Path;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use std::fmt::Debug;

use async_trait::async_trait;
use futures::stream;
use futures::SinkExt;
use tokio::net::TcpListener;
use tracing::{error, info};
use turso_core::copy::encode_copy_binary_file;
use turso_core::{validate_schema_name, Connection, LimboError, StepResult, Value};
use turso_parser_pg::translator::{
    try_extract_copy_stdin, try_extract_copy_stdout, PgCopyFormat, PgCopyStdinStmt,
    PgCopyStdoutStmt,
};

use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::auth::StartupHandler;
use pgwire::api::cancel::CancelHandler;
use pgwire::api::copy::{send_copy_out_response, CopyHandler};
use pgwire::api::portal::{Format, Portal};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    CopyResponse, DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat,
    FieldInfo, QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::{ClientInfo, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::cancel::CancelRequest;
use pgwire::messages::copy::{CopyData, CopyDone};
use pgwire::messages::data::DataRow;
use pgwire::messages::response::CommandComplete;
use pgwire::messages::startup::SecretKey;
use pgwire::messages::PgWireBackendMessage;
use pgwire::tokio::process_socket;
use pgwire::types::format::FormatOptions;

pub struct TursoPgServer {
    address: String,
    db_file: String,
    conn: Arc<Mutex<Arc<Connection>>>,
    interrupt_count: Arc<AtomicUsize>,
    tls_acceptor: Option<pgwire::tokio::TlsAcceptor>,
    cancel_registry: Arc<TursoCancelRegistry>,
}

impl TursoPgServer {
    pub fn new(
        address: String,
        db_file: String,
        conn: Arc<Connection>,
        interrupt_count: Arc<AtomicUsize>,
        tls_cert: Option<&Path>,
        tls_key: Option<&Path>,
    ) -> anyhow::Result<Self> {
        // Set postgres dialect on the connection
        conn.set_sql_dialect(turso_core::SqlDialect::Postgres);

        let tls_acceptor = match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => Some(load_tls_acceptor(cert, key)?),
            (None, None) => None,
            _ => {
                anyhow::bail!("--tls-cert and --tls-key must be specified together");
            }
        };

        Ok(Self {
            address,
            db_file,
            conn: Arc::new(Mutex::new(conn)),
            interrupt_count,
            tls_acceptor,
            cancel_registry: Arc::new(TursoCancelRegistry::default()),
        })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(self.run_async())
    }

    async fn run_async(&self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.address).await?;
        println!(
            "PostgreSQL server listening on {} (database: {})",
            self.address, self.db_file
        );

        let tls_acceptor = self.tls_acceptor.clone();
        let factory = Arc::new(TursoPgFactory {
            handler: Arc::new(TursoPgHandler {
                conn: self.conn.clone(),
                db_file: self.db_file.clone(),
                query_parser: Arc::new(NoopQueryParser::new()),
                copy_in: Arc::new(Mutex::new(CopyInSession::default())),
            }),
            cancel_registry: self.cancel_registry.clone(),
        });

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((socket, addr)) => {
                            info!("PostgreSQL client connected from {}", addr);
                            let factory_ref = factory.clone();
                            let tls = tls_acceptor.clone();
                            tokio::spawn(async move {
                                if let Err(e) = process_socket(socket, tls, factory_ref).await {
                                    error!("Error processing connection from {}: {}", addr, e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Error accepting connection: {}", e);
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    println!("\nShutting down PostgreSQL server...");
                    break;
                }
            }

            if self.interrupt_count.load(Ordering::SeqCst) > 0 {
                println!("Shutting down PostgreSQL server...");
                break;
            }
        }

        Ok(())
    }
}

#[derive(Default)]
struct TursoCancelRegistry {
    sessions: Mutex<HashMap<(i32, SecretKey), Arc<Connection>>>,
}

impl TursoCancelRegistry {
    fn register(&self, pid: i32, secret: SecretKey, conn: Arc<Connection>) {
        self.sessions.lock().unwrap().insert((pid, secret), conn);
    }
}

struct TursoCancelHandler {
    registry: Arc<TursoCancelRegistry>,
}

#[async_trait]
impl CancelHandler for TursoCancelHandler {
    async fn on_cancel_request(&self, cancel_request: CancelRequest) {
        let key = (cancel_request.pid, cancel_request.secret_key);
        if let Some(conn) = self.registry.sessions.lock().unwrap().get(&key) {
            conn.interrupt();
        }
    }
}

struct TursoStartupHandler {
    conn: Arc<Connection>,
    registry: Arc<TursoCancelRegistry>,
}

#[async_trait]
impl NoopStartupHandler for TursoStartupHandler {
    async fn post_startup<C>(
        &self,
        client: &mut C,
        _message: pgwire::messages::PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: pgwire::api::ClientInfo + futures::Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as futures::Sink<PgWireBackendMessage>>::Error>,
    {
        let (pid, secret) = client.pid_and_secret_key();
        self.registry.register(pid, secret, self.conn.clone());
        Ok(())
    }
}

#[derive(Default)]
struct CopyInSession {
    stmt: Option<PgCopyStdinStmt>,
    buffer: Vec<u8>,
}

struct TursoPgHandler {
    conn: Arc<Mutex<Arc<Connection>>>,
    db_file: String,
    query_parser: Arc<NoopQueryParser>,
    copy_in: Arc<Mutex<CopyInSession>>,
}

impl TursoPgHandler {
    /// After a DROP SCHEMA query succeeds, delete the schema's database file.
    fn cleanup_dropped_schema_file(&self, query: &str, portal: Option<&Portal<String>>) {
        if self.db_file == ":memory:" {
            return;
        }
        let bound_params: Vec<Option<&[u8]>> = match portal {
            Some(p) => (0..p.parameter_len())
                .map(|i| p.parameters[i].as_deref())
                .collect(),
            None => Vec::new(),
        };
        let Some(name) = drop_schema_name(query, &bound_params) else {
            return;
        };
        delete_schema_file(&self.db_file, &name);
    }
}

/// Extract the schema name from a DROP SCHEMA statement.
/// Resolves `$N` placeholders from bound extended-protocol parameters.
fn drop_schema_name(query: &str, bound_params: &[Option<&[u8]>]) -> Option<String> {
    let trimmed = query.trim().trim_end_matches(';').to_lowercase();
    if !trimmed.starts_with("drop schema") {
        return None;
    }
    let rest = trimmed.strip_prefix("drop schema")?.trim();
    let rest = rest
        .strip_prefix("if exists")
        .map(|s| s.trim())
        .unwrap_or(rest);
    let token = rest.split_whitespace().next()?;
    resolve_drop_schema_token(token, bound_params)
}

fn resolve_drop_schema_token(token: &str, bound_params: &[Option<&[u8]>]) -> Option<String> {
    let name = if let Some(idx_str) = token.strip_prefix('$') {
        let idx: usize = idx_str.parse().ok()?;
        if idx == 0 {
            return None;
        }
        let bytes = bound_params.get(idx - 1)?.as_ref()?;
        std::str::from_utf8(bytes).ok()?
    } else {
        token
    };
    let name = name.trim_matches('"');
    if name.is_empty() || name.eq_ignore_ascii_case("public") {
        return None;
    }
    validate_schema_name(name).ok()?;
    Some(name.to_owned())
}

fn delete_schema_file(db_file: &str, name: &str) {
    let parent = std::path::Path::new(db_file)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let schema_file = parent.join(format!("turso-postgres-schema-{name}.db"));
    if schema_file.exists() {
        if let Err(e) = std::fs::remove_file(&schema_file) {
            tracing::warn!("Failed to delete schema file {:?}: {}", schema_file, e);
        } else {
            tracing::info!("Deleted schema file {:?}", schema_file);
        }
        let wal = schema_file.with_extension("db-wal");
        let shm = schema_file.with_extension("db-shm");
        let _ = std::fs::remove_file(wal);
        let _ = std::fs::remove_file(shm);
    }
}

struct TursoPgFactory {
    handler: Arc<TursoPgHandler>,
    cancel_registry: Arc<TursoCancelRegistry>,
}

impl PgWireServerHandlers for TursoPgFactory {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.handler.clone()
    }

    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        Arc::new(TursoStartupHandler {
            conn: self.handler.conn.lock().unwrap().clone(),
            registry: self.cancel_registry.clone(),
        })
    }

    fn copy_handler(&self) -> Arc<impl CopyHandler> {
        self.handler.clone()
    }

    fn cancel_handler(&self) -> Arc<impl CancelHandler> {
        Arc::new(TursoCancelHandler {
            registry: self.cancel_registry.clone(),
        })
    }
}

fn load_tls_acceptor(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<pgwire::tokio::TlsAcceptor> {
    use rustls_pemfile::{certs, pkcs8_private_keys};
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;

    let certs = certs(&mut BufReader::new(File::open(cert_path)?))
        .collect::<Result<Vec<CertificateDer>, IoError>>()?;
    let key = pkcs8_private_keys(&mut BufReader::new(File::open(key_path)?))
        .map(|key| key.map(PrivateKeyDer::from))
        .collect::<Result<Vec<PrivateKeyDer>, IoError>>()?
        .into_iter()
        .next()
        .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "no private key found"))?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|err| IoError::new(ErrorKind::InvalidInput, err))?;
    config.alpn_protocols = vec![b"postgresql".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[async_trait]
impl SimpleQueryHandler for TursoPgHandler {
    async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + Unpin + Send + Sync,
        C: futures::Sink<PgWireBackendMessage> + Unpin,
        C::Error: Debug,
        PgWireError: From<<C as futures::Sink<PgWireBackendMessage>>::Error>,
    {
        let conn = self.conn.lock().unwrap().clone();

        // Per the PostgreSQL simple query protocol, a query string may contain
        // multiple semicolon-separated statements. Split and execute each one.
        let statements = turso_parser_pg::split_statements(query)
            .map_err(|e| PgWireError::UserError(Box::new(parse_error_info(&e.to_string()))))?;

        let mut responses = Vec::new();
        for sql in &statements {
            if let Some(copy) = parse_copy_stdin(sql) {
                let cols = copy_column_count(&conn, &copy)?;
                *self.copy_in.lock().unwrap() = CopyInSession {
                    stmt: Some(copy),
                    buffer: Vec::new(),
                };
                responses.push(Response::CopyIn(CopyResponse::new(0, cols, vec![0; cols])));
                continue;
            }
            if let Some(copy) = parse_copy_stdout(sql) {
                responses.extend(handle_copy_stdout(client, &conn, &copy).await?);
                continue;
            }

            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

            if stmt.num_columns() == 0 || is_pg_non_query(sql) {
                responses.push(execute_non_query(&mut stmt, sql)?);
            } else {
                let header = Arc::new(build_field_info(&stmt, &Format::UnifiedText));
                responses.push(execute_query(stmt, header)?);
            }

            // Only delete the backing schema file once the statement has
            // executed successfully. Deleting before execution risks orphaning
            // schema metadata if execution fails.
            self.cleanup_dropped_schema_file(sql, None);
        }

        Ok(responses)
    }
}

#[async_trait]
impl ExtendedQueryHandler for TursoPgHandler {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn do_query<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + Unpin + Send + Sync,
        C: futures::Sink<PgWireBackendMessage> + Unpin,
        C::Error: Debug,
        PgWireError: From<<C as futures::Sink<PgWireBackendMessage>>::Error>,
    {
        let conn = self.conn.lock().unwrap().clone();
        let query = &portal.statement.statement;

        if let Some(copy) = parse_copy_stdin(query) {
            let cols = copy_column_count(&conn, &copy)?;
            *self.copy_in.lock().unwrap() = CopyInSession {
                stmt: Some(copy),
                buffer: Vec::new(),
            };
            return Ok(Response::CopyIn(CopyResponse::new(0, cols, vec![0; cols])));
        }
        if let Some(copy) = parse_copy_stdout(query) {
            let mut responses = handle_copy_stdout(client, &conn, &copy).await?;
            return responses
                .pop()
                .ok_or_else(|| PgWireError::ApiError("COPY TO produced no response".into()));
        }

        let mut stmt = conn
            .prepare(query)
            .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

        // Bind parameters from the portal
        bind_portal_parameters(&mut stmt, portal)?;

        if stmt.num_columns() == 0 || is_pg_non_query(query) {
            let response = execute_non_query(&mut stmt, query)?;
            // Delete the backing schema file only after successful execution.
            self.cleanup_dropped_schema_file(query, Some(portal));
            return Ok(response);
        }

        let header = Arc::new(build_field_info(&stmt, &portal.result_column_format));
        execute_query(stmt, header)
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        target: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let conn = self.conn.lock().unwrap().clone();
        let stmt = conn
            .prepare(&target.statement)
            .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

        let param_types: Vec<Type> = target
            .parameter_types
            .iter()
            .map(|t| t.clone().unwrap_or(Type::TEXT))
            .collect();

        let fields = build_field_info(&stmt, &Format::UnifiedText);
        Ok(DescribeStatementResponse::new(param_types, fields))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let conn = self.conn.lock().unwrap().clone();
        let stmt = conn
            .prepare(&portal.statement.statement)
            .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

        let fields = build_field_info(&stmt, &portal.result_column_format);
        Ok(DescribePortalResponse::new(fields))
    }
}

/// Build FieldInfo metadata from a prepared statement's column information.
fn build_field_info(stmt: &turso_core::Statement, format: &Format) -> Vec<FieldInfo> {
    (0..stmt.num_columns())
        .map(|i| {
            let name = stmt.get_column_name(i).into_owned();
            // Use the declared column type if available, falling back to TEXT.
            // The actual runtime value type may differ (e.g. raw SQL subqueries),
            // so encode_value() also handles type mismatches.
            let mut pg_type = stmt
                .get_column_decltype(i)
                .or_else(|| stmt.get_column_inferred_type(i))
                .map(|t| {
                    let mapped = sqlite_type_to_pg_type(&t);
                    // SQLite infers "BLOB" for expressions with no affinity (e.g.
                    // function calls in subqueries). For PG wire, BYTEA causes
                    // clients to return raw Buffer objects. Map inferred BLOB
                    // (not explicit BYTEA) to TEXT.
                    if mapped == Type::BYTEA && t.eq_ignore_ascii_case("BLOB") {
                        Type::TEXT
                    } else {
                        mapped
                    }
                })
                .unwrap_or(Type::TEXT);
            // If the column is an array, promote scalar type to array type
            if let Some(dims) = stmt.get_column_array_dimensions(i) {
                if dims > 0 {
                    pg_type = scalar_pg_type_to_array_type(&pg_type);
                }
            }
            FieldInfo::new(name, None, None, pg_type, format.format_for(i))
        })
        .collect()
}

/// Map a scalar PG type to its array counterpart.
fn scalar_pg_type_to_array_type(scalar: &Type) -> Type {
    if *scalar == Type::INT4 {
        Type::INT4_ARRAY
    } else if *scalar == Type::INT8 {
        Type::INT8_ARRAY
    } else if *scalar == Type::FLOAT8 {
        Type::FLOAT8_ARRAY
    } else if *scalar == Type::BOOL {
        Type::BOOL_ARRAY
    } else if *scalar == Type::TEXT || *scalar == Type::VARCHAR {
        Type::TEXT_ARRAY
    } else if *scalar == Type::UUID {
        Type::UUID_ARRAY
    } else if *scalar == Type::JSON {
        Type::JSON_ARRAY
    } else if *scalar == Type::JSONB {
        Type::JSONB_ARRAY
    } else if *scalar == Type::DATE {
        Type::DATE_ARRAY
    } else if *scalar == Type::TIME {
        Type::TIME_ARRAY
    } else if *scalar == Type::TIMESTAMP {
        Type::TIMESTAMP_ARRAY
    } else if *scalar == Type::TIMESTAMPTZ {
        Type::TIMESTAMPTZ_ARRAY
    } else if *scalar == Type::INET {
        Type::INET_ARRAY
    } else if *scalar == Type::CIDR {
        Type::CIDR_ARRAY
    } else if *scalar == Type::MACADDR {
        Type::MACADDR_ARRAY
    } else if *scalar == Type::MACADDR8 {
        Type::MACADDR8_ARRAY
    } else if *scalar == Type::NUMERIC {
        Type::NUMERIC_ARRAY
    } else if *scalar == Type::BYTEA {
        Type::BYTEA_ARRAY
    } else if *scalar == Type::FLOAT4 {
        Type::FLOAT4_ARRAY
    } else {
        Type::TEXT_ARRAY
    }
}

fn parse_copy_stdin(sql: &str) -> Option<PgCopyStdinStmt> {
    turso_parser_pg::parse(sql)
        .ok()
        .and_then(|parsed| try_extract_copy_stdin(&parsed))
}

fn parse_copy_stdout(sql: &str) -> Option<PgCopyStdoutStmt> {
    turso_parser_pg::parse(sql)
        .ok()
        .and_then(|parsed| try_extract_copy_stdout(&parsed))
}

fn copy_column_count(conn: &Arc<Connection>, copy: &PgCopyStdinStmt) -> PgWireResult<usize> {
    if let Some(cols) = &copy.columns {
        return Ok(cols.len());
    }
    let count = conn
        .get_table_columns(&copy.table_name, copy.schema_name.as_deref())
        .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?
        .len();
    if count == 0 {
        return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(
            LimboError::ParseError(format!(
                "COPY: table '{}' not found or has no columns",
                copy.table_name
            )),
        ))));
    }
    Ok(count)
}

async fn handle_copy_stdout<C>(
    client: &mut C,
    conn: &Arc<Connection>,
    copy: &PgCopyStdoutStmt,
) -> PgWireResult<Vec<Response>>
where
    C: futures::Sink<PgWireBackendMessage> + Unpin,
    C::Error: Debug,
    PgWireError: From<<C as futures::Sink<PgWireBackendMessage>>::Error>,
{
    let columns = if let Some(cols) = &copy.columns {
        cols.clone()
    } else {
        conn.get_table_columns(&copy.table_name, copy.schema_name.as_deref())
            .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?
    };
    if columns.is_empty() {
        return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(
            LimboError::ParseError(format!(
                "COPY: table '{}' not found or has no columns",
                copy.table_name
            )),
        ))));
    }

    let qualified = match &copy.schema_name {
        Some(schema) => format!("\"{schema}\".\"{}\"", copy.table_name),
        None => format!("\"{}\"", copy.table_name),
    };
    let col_list = columns
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {col_list} FROM {qualified}");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

    let mut rows = Vec::new();
    loop {
        match stmt.step() {
            Ok(StepResult::Row) => {
                let row = stmt.row().expect("row after StepResult::Row");
                let values: Vec<String> = row
                    .get_values()
                    .map(|v| format_copy_field(v, &copy.null_string))
                    .collect();
                rows.push(values);
            }
            Ok(StepResult::IO) => stmt
                .get_pager()
                .io
                .step()
                .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?,
            Ok(StepResult::Done) => break,
            Ok(StepResult::Interrupt) => {
                return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(
                    LimboError::Interrupt,
                ))));
            }
            Ok(StepResult::Busy) => {
                return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(
                    LimboError::Busy,
                ))));
            }
            Err(e) => {
                return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(e))));
            }
        }
    }

    send_copy_out_response(
        client,
        CopyResponse::new(0, columns.len(), vec![0; columns.len()]),
    )
    .await?;

    match copy.format {
        PgCopyFormat::Text => {
            for row in &rows {
                let mut line = String::new();
                for (i, val) in row.iter().enumerate() {
                    if i > 0 {
                        line.push(copy.delimiter);
                    }
                    line.push_str(val);
                }
                line.push('\n');
                client
                    .send(PgWireBackendMessage::CopyData(CopyData::new(
                        line.into_bytes().into(),
                    )))
                    .await?;
            }
        }
        PgCopyFormat::Binary => {
            let encoded_rows: Vec<Vec<Option<String>>> = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|v| {
                            if v == &copy.null_string {
                                None
                            } else {
                                Some(v.clone())
                            }
                        })
                        .collect()
                })
                .collect();
            let payload = encode_copy_binary_file(&encoded_rows);
            client
                .send(PgWireBackendMessage::CopyData(CopyData::new(
                    payload.into(),
                )))
                .await?;
        }
    }
    client
        .send(PgWireBackendMessage::CopyDone(CopyDone::new()))
        .await?;

    Ok(vec![Response::Execution(
        Tag::new("COPY").with_rows(rows.len()),
    )])
}

fn format_copy_field(val: &Value, null_string: &str) -> String {
    match val {
        Value::Null => null_string.to_string(),
        Value::Text(t) => t.as_str().to_string(),
        other => other.to_string(),
    }
}

#[async_trait]
impl CopyHandler for TursoPgHandler {
    async fn on_copy_data<C>(&self, _client: &mut C, copy_data: CopyData) -> PgWireResult<()>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        self.copy_in
            .lock()
            .unwrap()
            .buffer
            .extend_from_slice(&copy_data.data);
        Ok(())
    }

    async fn on_copy_done<C>(&self, client: &mut C, _done: CopyDone) -> PgWireResult<()>
    where
        C: ClientInfo + Unpin + Send + Sync,
        C: futures::Sink<PgWireBackendMessage> + Unpin,
        C::Error: Debug,
        PgWireError: From<<C as futures::Sink<PgWireBackendMessage>>::Error>,
    {
        let (copy, data) = {
            let mut session = self.copy_in.lock().unwrap();
            let Some(copy) = session.stmt.take() else {
                return Err(PgWireError::UserError(Box::new(pg_error_info(
                    "COPY FROM STDIN without active copy session".into(),
                    "57014",
                ))));
            };
            let data = std::mem::take(&mut session.buffer);
            (copy, data)
        };

        let conn = self.conn.lock().unwrap().clone();
        let rows = conn
            .handle_pg_copy_data(
                &copy.table_name,
                copy.schema_name.as_deref(),
                copy.columns.as_deref(),
                copy.format,
                copy.delimiter.as_deref(),
                copy.header,
                copy.null_string.as_deref(),
                &data,
            )
            .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

        client
            .send(PgWireBackendMessage::CommandComplete(CommandComplete::new(
                format!("COPY {rows}"),
            )))
            .await?;
        Ok(())
    }
}

struct QueryStreamState {
    stmt: turso_core::Statement,
    header: Arc<Vec<FieldInfo>>,
}

fn encode_row(row: &turso_core::Row, header: &Arc<Vec<FieldInfo>>) -> turso_core::Result<DataRow> {
    let mut encoder = DataRowEncoder::new(header.clone());
    for (i, val) in row.get_values().enumerate() {
        let pg_type = header
            .get(i)
            .map(|fi| fi.datatype().clone())
            .unwrap_or(Type::TEXT);
        encode_value(&mut encoder, val, &pg_type)?;
    }
    encoder
        .finish()
        .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
}

/// Execute a query that returns rows and build a lazily-streamed Query response.
fn execute_query(
    stmt: turso_core::Statement,
    header: Arc<Vec<FieldInfo>>,
) -> PgWireResult<Response> {
    let row_stream =
        stream::try_unfold(
            Some(QueryStreamState {
                stmt,
                header: header.clone(),
            }),
            |state| async move {
                let mut state = match state {
                    Some(s) => s,
                    None => return Ok(None),
                };
                loop {
                    match state.stmt.step() {
                        Ok(StepResult::Row) => {
                            let row = state
                                .stmt
                                .row()
                                .expect("row must be present after StepResult::Row");
                            let data_row = encode_row(row, &state.header).map_err(|e| {
                                PgWireError::UserError(Box::new(limbo_error_to_pg(e)))
                            })?;
                            return Ok(Some((data_row, Some(state))));
                        }
                        Ok(StepResult::IO) => {
                            state.stmt.get_pager().io.step().map_err(|e| {
                                PgWireError::UserError(Box::new(limbo_error_to_pg(e)))
                            })?;
                        }
                        Ok(StepResult::Done) => return Ok(None),
                        Ok(StepResult::Interrupt) => {
                            return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(
                                turso_core::LimboError::Interrupt,
                            ))));
                        }
                        Ok(StepResult::Busy) => {
                            return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(
                                turso_core::LimboError::Busy,
                            ))));
                        }
                        Err(e) => {
                            return Err(PgWireError::UserError(Box::new(limbo_error_to_pg(e))));
                        }
                    }
                }
            },
        );

    Ok(Response::Query(QueryResponse::new(header, row_stream)))
}

/// Execute a non-SELECT statement and build an Execution response.
fn execute_non_query(stmt: &mut turso_core::Statement, query: &str) -> PgWireResult<Response> {
    stmt.run_ignore_rows()
        .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

    let affected = stmt.n_change();
    let tag = command_tag(query, affected as usize);
    Ok(Response::Execution(tag))
}

/// Extract parameters from a Portal and bind them to a prepared statement.
///
/// PostgreSQL parameters ($1, $2, ...) map to portal parameters 0, 1, ...
/// The bytecode compiler may allocate internal parameter indices in a different
/// order than the $N numbering (e.g. if $2 appears before $1 in the SQL), so we
/// look up each parameter's internal index by name.
fn bind_portal_parameters(
    stmt: &mut turso_core::Statement,
    portal: &Portal<String>,
) -> PgWireResult<()> {
    for i in 0..portal.parameter_len() {
        let value = match &portal.parameters[i] {
            None => Value::Null,
            Some(bytes) => {
                let pg_type = portal
                    .statement
                    .parameter_types
                    .get(i)
                    .and_then(|t| t.as_ref())
                    .unwrap_or(&Type::UNKNOWN);
                let format = portal.parameter_format.format_for(i);
                pg_bytes_to_value(bytes, pg_type, format)?
            }
        };
        // Portal parameter i corresponds to PostgreSQL $N where N = i + 1.
        // Look up the internal index that the bytecode compiler assigned to $N.
        let pg_param_name = format!("${}", i + 1);
        let idx = stmt
            .parameter_index(&pg_param_name)
            .unwrap_or_else(|| NonZero::new(i + 1).expect("parameter index must be non-zero"));
        stmt.bind_at(idx, value);
    }
    Ok(())
}

/// Convert raw parameter bytes to a turso Value based on the PostgreSQL type and
/// wire format (text or binary).
fn pg_bytes_to_value(bytes: &[u8], pg_type: &Type, format: FieldFormat) -> PgWireResult<Value> {
    match format {
        FieldFormat::Text => pg_bytes_to_value_text(bytes, pg_type),
        FieldFormat::Binary => pg_bytes_to_value_binary(bytes, pg_type),
    }
}

fn binary_parameter_error(message: impl std::fmt::Display) -> PgWireError {
    PgWireError::UserError(Box::new(invalid_parameter_error(format!(
        "invalid binary parameter: {message}"
    ))))
}

fn read_be_i16(bytes: &[u8]) -> Result<i16, String> {
    let arr: [u8; 2] = bytes
        .try_into()
        .map_err(|_| format!("expected 2 bytes, got {}", bytes.len()))?;
    Ok(i16::from_be_bytes(arr))
}

fn read_be_i32(bytes: &[u8]) -> Result<i32, String> {
    let arr: [u8; 4] = bytes
        .try_into()
        .map_err(|_| format!("expected 4 bytes, got {}", bytes.len()))?;
    Ok(i32::from_be_bytes(arr))
}

fn read_be_i64(bytes: &[u8]) -> Result<i64, String> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| format!("expected 8 bytes, got {}", bytes.len()))?;
    Ok(i64::from_be_bytes(arr))
}

fn read_be_f32(bytes: &[u8]) -> Result<f32, String> {
    let arr: [u8; 4] = bytes
        .try_into()
        .map_err(|_| format!("expected 4 bytes, got {}", bytes.len()))?;
    Ok(f32::from_be_bytes(arr))
}

fn read_be_f64(bytes: &[u8]) -> Result<f64, String> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| format!("expected 8 bytes, got {}", bytes.len()))?;
    Ok(f64::from_be_bytes(arr))
}

/// Proleptic Gregorian calendar helpers (Howard Hinnant).
fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let mut y = year;
    y -= if month <= 2 { 1 } else { 0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let month_adj: i32 = if month > 2 {
        i32::try_from(month).unwrap_or(12) - 3
    } else {
        i32::try_from(month).unwrap_or(1) + 9
    };
    let doy = (153 * month_adj + 2) / 5 + i32::try_from(day).unwrap_or(1) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = i64::from(z);
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (
        i32::try_from(y).unwrap_or(i32::MAX),
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

fn format_pg_date(days_since_2000: i32) -> String {
    let (y, m, d) = civil_from_days(days_from_civil(2000, 1, 1) + days_since_2000);
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_pg_time_micros(micros: i64) -> String {
    let micros = micros.rem_euclid(86_400_000_000);
    let secs = micros / 1_000_000;
    let frac = micros % 1_000_000;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;
    if frac == 0 {
        format!("{hours:02}:{mins:02}:{secs:02}")
    } else {
        let frac_str = format!("{frac:06}");
        let trimmed = frac_str.trim_end_matches('0');
        format!("{hours:02}:{mins:02}:{secs:02}.{trimmed}")
    }
}

fn format_pg_timestamp_micros(micros: i64) -> String {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    let days = micros.div_euclid(MICROS_PER_DAY);
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    format!(
        "{} {}",
        format_pg_date(days as i32),
        format_pg_time_micros(day_micros)
    )
}

fn format_uuid(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() != 16 {
        return Err(format!("expected 16 bytes, got {}", bytes.len()));
    }
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(bytes[0..4].try_into().unwrap()),
        u16::from_be_bytes(bytes[4..6].try_into().unwrap()),
        u16::from_be_bytes(bytes[6..8].try_into().unwrap()),
        u16::from_be_bytes(bytes[8..10].try_into().unwrap()),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ])
    ))
}

fn format_pg_interval_binary(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() != 16 {
        return Err(format!("expected 16 bytes, got {}", bytes.len()));
    }
    let micros = read_be_i64(&bytes[0..8])?;
    let days = read_be_i32(&bytes[8..12])?;
    let months = read_be_i32(&bytes[12..16])?;
    let mut parts = Vec::new();
    if months != 0 {
        parts.push(format!("{months} mons"));
    }
    if days != 0 {
        parts.push(format!("{days} days"));
    }
    if micros != 0 {
        let secs = micros / 1_000_000;
        let frac = micros % 1_000_000;
        if frac == 0 {
            parts.push(format!("{secs} secs"));
        } else {
            let frac_str = format!("{frac:06}").trim_end_matches('0').to_string();
            parts.push(format!("{secs}.{frac_str} secs"));
        }
    }
    if parts.is_empty() {
        Ok("0".to_string())
    } else {
        Ok(parts.join(" "))
    }
}

fn format_money_cents(cents: i64) -> String {
    let negative = cents < 0;
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let frac = abs % 100;
    if negative {
        format!("-${dollars}.{frac:02}")
    } else {
        format!("${dollars}.{frac:02}")
    }
}

/// Decode a parameter sent in PostgreSQL binary format.
fn pg_bytes_to_value_binary(bytes: &[u8], pg_type: &Type) -> PgWireResult<Value> {
    match *pg_type {
        Type::INT2 => {
            let v = read_be_i16(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_i64(i64::from(v)))
        }
        Type::INT4 => {
            let v = read_be_i32(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_i64(i64::from(v)))
        }
        Type::INT8 => {
            let v = read_be_i64(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_i64(v))
        }
        Type::FLOAT4 => {
            let v = read_be_f32(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_f64(f64::from(v)))
        }
        Type::FLOAT8 => {
            let v = read_be_f64(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_f64(v))
        }
        Type::BOOL => {
            if bytes.len() != 1 {
                return Err(binary_parameter_error(format!(
                    "expected 1 byte, got {}",
                    bytes.len()
                )));
            }
            Ok(Value::from_i64(if bytes[0] == 0 { 0 } else { 1 }))
        }
        Type::BYTEA => Ok(Value::from_blob(bytes.to_vec())),
        Type::NUMERIC => {
            let text = turso_core::pg_wire_numeric_binary_to_text(bytes)
                .map_err(|e| binary_parameter_error(e.to_string()))?;
            Ok(Value::from_text(text))
        }
        Type::DATE => {
            let days = read_be_i32(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_text(format_pg_date(days)))
        }
        Type::TIME => {
            let micros = read_be_i64(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_text(format_pg_time_micros(micros)))
        }
        Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            let micros = read_be_i64(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_text(format_pg_timestamp_micros(micros)))
        }
        Type::UUID => {
            let text = format_uuid(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_text(text))
        }
        Type::INTERVAL => {
            let text = format_pg_interval_binary(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_text(text))
        }
        Type::MONEY => {
            let cents = read_be_i64(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_text(format_money_cents(cents)))
        }
        Type::UNKNOWN => pg_bytes_to_value_binary_unknown(bytes),
        // TEXT, VARCHAR, and other types: binary is still UTF-8 payload
        _ => {
            let text = std::str::from_utf8(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_text(text.to_owned()))
        }
    }
}

/// Infer type for UNKNOWN binary parameters when the client omits explicit OIDs.
fn pg_bytes_to_value_binary_unknown(bytes: &[u8]) -> PgWireResult<Value> {
    if std::str::from_utf8(bytes).is_ok() {
        return pg_bytes_to_value_text(bytes, &Type::UNKNOWN);
    }

    match bytes.len() {
        1 => Ok(Value::from_i64(if bytes[0] == 0 { 0 } else { 1 })),
        2 => {
            let v = read_be_i16(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_i64(i64::from(v)))
        }
        4 => {
            let v = read_be_i32(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_i64(i64::from(v)))
        }
        8 => {
            let v = read_be_i64(bytes).map_err(binary_parameter_error)?;
            Ok(Value::from_i64(v))
        }
        len => Err(binary_parameter_error(format!(
            "cannot infer UNKNOWN binary parameter from {len} bytes"
        ))),
    }
}

/// Convert raw parameter bytes to a turso Value based on the PostgreSQL type.
/// Assumes text format encoding (UTF-8 string representations).
fn pg_bytes_to_value_text(bytes: &[u8], pg_type: &Type) -> PgWireResult<Value> {
    let text = std::str::from_utf8(bytes).map_err(|e| {
        PgWireError::UserError(Box::new(invalid_parameter_error(format!(
            "invalid UTF-8 in parameter: {e}"
        ))))
    })?;

    match *pg_type {
        Type::INT2 | Type::INT4 | Type::INT8 => {
            let i: i64 = text.parse().map_err(|e| {
                PgWireError::UserError(Box::new(invalid_parameter_error(format!(
                    "invalid integer parameter: {e}"
                ))))
            })?;
            Ok(Value::from_i64(i))
        }
        Type::NUMERIC => Ok(Value::from_text(text.to_owned())),
        Type::FLOAT4 | Type::FLOAT8 => {
            let f: f64 = text.parse().map_err(|e| {
                PgWireError::UserError(Box::new(invalid_parameter_error(format!(
                    "invalid float parameter: {e}"
                ))))
            })?;
            Ok(Value::from_f64(f))
        }
        Type::BOOL => match text {
            "t" | "true" | "TRUE" | "1" | "yes" | "on" => Ok(Value::from_i64(1)),
            "f" | "false" | "FALSE" | "0" | "no" | "off" => Ok(Value::from_i64(0)),
            _ => Err(PgWireError::UserError(Box::new(invalid_parameter_error(
                format!("invalid boolean parameter: {text}"),
            )))),
        },
        Type::BYTEA => {
            // PostgreSQL text format for bytea uses \x hex encoding
            if let Some(hex_str) = text.strip_prefix("\\x") {
                let data = decode_hex(hex_str).map_err(|e| {
                    PgWireError::UserError(Box::new(invalid_parameter_error(format!(
                        "invalid bytea hex parameter: {e}"
                    ))))
                })?;
                Ok(Value::from_blob(data))
            } else {
                // Raw bytes as-is
                Ok(Value::from_blob(bytes.to_vec()))
            }
        }
        // UNKNOWN: try to infer type from text content (numeric-looking values
        // should be bound as numbers so comparisons with COUNT/SUM etc. work)
        Type::UNKNOWN => {
            if let Ok(i) = text.parse::<i64>() {
                Ok(Value::from_i64(i))
            } else if let Ok(f) = text.parse::<f64>() {
                Ok(Value::from_f64(f))
            } else if text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("t") {
                Ok(Value::from_i64(1))
            } else if text.eq_ignore_ascii_case("false") || text.eq_ignore_ascii_case("f") {
                Ok(Value::from_i64(0))
            } else {
                Ok(Value::from_text(text.to_owned()))
            }
        }
        // TEXT, VARCHAR, and all other types → text
        _ => Ok(Value::from_text(text.to_owned())),
    }
}

/// Decode a hex string into bytes.
fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("odd-length hex string".to_owned());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| format!("invalid hex at position {i}: {e}"))
        })
        .collect()
}

fn encode_value(
    encoder: &mut DataRowEncoder,
    val: &Value,
    pg_type: &Type,
) -> turso_core::Result<()> {
    match val {
        Value::Null => encoder
            .encode_field(&None::<i8>)
            .map_err(|e| turso_core::LimboError::InternalError(e.to_string())),
        Value::Numeric(turso_core::Numeric::Integer(i)) => {
            // Boolean columns: encode as true/false instead of 0/1
            if *pg_type == Type::BOOL {
                encoder
                    .encode_field(&(*i != 0))
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            } else if *pg_type == Type::INT2 {
                let v = i16::try_from(*i).map_err(|_| turso_core::LimboError::IntegerOverflow)?;
                encoder
                    .encode_field(&v)
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            } else if *pg_type == Type::INT4 {
                let v = i32::try_from(*i).map_err(|_| turso_core::LimboError::IntegerOverflow)?;
                encoder
                    .encode_field(&v)
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            } else if *pg_type == Type::NUMERIC {
                let text = turso_core::value_to_pg_numeric_text(val)?;
                encoder
                    .encode_field_with_type_and_format(
                        &text.as_str(),
                        pg_type,
                        FieldFormat::Text,
                        &FormatOptions::default(),
                    )
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            } else {
                encoder
                    .encode_field(i)
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            }
        }
        Value::Numeric(turso_core::Numeric::Float(f)) => {
            if *pg_type == Type::NUMERIC {
                let text = turso_core::value_to_pg_numeric_text(val)?;
                encoder
                    .encode_field_with_type_and_format(
                        &text.as_str(),
                        pg_type,
                        FieldFormat::Text,
                        &FormatOptions::default(),
                    )
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            } else {
                encoder
                    .encode_field(&f64::from(*f))
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            }
        }
        Value::Text(t) => {
            let text = t.value.as_ref();
            // For TIMESTAMPTZ columns, ensure timezone info is present so clients
            // parse the value correctly (as UTC, not local time).
            // TIMESTAMP (without TZ) should NOT have timezone suffix.
            if *pg_type == Type::TIMESTAMPTZ
                && !text.contains('+')
                && !text.contains('Z')
                && !text.ends_with("-00")
            {
                let with_tz = format!("{text}+00");
                encoder
                    .encode_field(&with_tz.as_str())
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            } else if pg_type.name().starts_with('_') {
                // Array types: pgwire's to_sql_text quotes strings containing
                // {, }, or commas when the type is Kind::Array. Since we store
                // array values as pre-formatted PG array literals (e.g.
                // "{1,2,3}"), encode with Type::TEXT to bypass the quoting.
                encoder
                    .encode_field_with_type_and_format(
                        &text,
                        &Type::TEXT,
                        FieldFormat::Text,
                        &FormatOptions::default(),
                    )
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            } else {
                encoder
                    .encode_field(&text)
                    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
            }
        }
        Value::Blob(b) => encoder
            .encode_field(&b.as_slice())
            .map_err(|e| turso_core::LimboError::InternalError(e.to_string())),
    }
}

fn sqlite_type_to_pg_type(type_str: &str) -> Type {
    let upper = type_str.to_uppercase();
    match upper.as_str() {
        "INTEGER" | "INT" | "INT4" | "SMALLINT" | "INT2" | "SERIAL" | "SMALLSERIAL" => Type::INT4,
        "BIGINT" | "INT8" | "BIGSERIAL" => Type::INT8,
        "REAL" | "FLOAT" | "FLOAT4" | "FLOAT8" | "DOUBLE" | "DOUBLE PRECISION" => Type::FLOAT8,
        "NUMERIC" | "DECIMAL" => Type::NUMERIC,
        "TEXT" | "VARCHAR" | "CHAR" | "CHARACTER VARYING" | "CHARACTER" | "NAME" => Type::TEXT,
        "BLOB" | "BYTEA" => Type::BYTEA,
        "BOOLEAN" | "BOOL" => Type::BOOL,
        "UUID" => Type::UUID,
        "JSON" => Type::JSON,
        "JSONB" => Type::JSONB,
        "DATE" => Type::DATE,
        "TIME" | "TIMETZ" => Type::TIME,
        "TIMESTAMP" => Type::TIMESTAMP,
        "TIMESTAMPTZ" => Type::TIMESTAMPTZ,
        "INTERVAL" => Type::INTERVAL,
        "MONEY" => Type::MONEY,
        "INET" => Type::INET,
        "CIDR" => Type::CIDR,
        "MACADDR" => Type::MACADDR,
        "MACADDR8" => Type::MACADDR8,
        _ => {
            // Handle parameterized types like varchar(50), numeric(10,2)
            if upper.starts_with("VARCHAR") || upper.starts_with("CHAR") {
                Type::VARCHAR
            } else if upper.starts_with("NUMERIC") || upper.starts_with("DECIMAL") {
                Type::NUMERIC
            } else {
                Type::TEXT
            }
        }
    }
}

/// PG statements handled by `try_prepare_pg()` that return a dummy SELECT
/// but should produce a command-tag response, not a result set.
fn is_pg_non_query(sql: &str) -> bool {
    let upper = sql.trim().to_uppercase();
    upper.starts_with("COPY")
        || upper.starts_with("CREATE SCHEMA")
        || upper.starts_with("DROP SCHEMA")
        || upper.starts_with("REFRESH MATERIALIZED VIEW")
}

fn command_tag(query: &str, affected_rows: usize) -> Tag {
    let upper = query.trim().to_uppercase();
    if upper.starts_with("INSERT") {
        Tag::new("INSERT").with_oid(0).with_rows(affected_rows)
    } else if upper.starts_with("UPDATE") {
        Tag::new("UPDATE").with_rows(affected_rows)
    } else if upper.starts_with("DELETE") || upper.starts_with("TRUNCATE") {
        Tag::new("DELETE").with_rows(affected_rows)
    } else if upper.starts_with("CREATE VIEW") {
        Tag::new("CREATE VIEW")
    } else if upper.starts_with("CREATE INDEX") {
        Tag::new("CREATE INDEX")
    } else if upper.starts_with("CREATE SCHEMA") {
        Tag::new("CREATE SCHEMA")
    } else if upper.starts_with("CREATE") {
        Tag::new("CREATE TABLE")
    } else if upper.starts_with("DROP VIEW") {
        Tag::new("DROP VIEW")
    } else if upper.starts_with("DROP INDEX") {
        Tag::new("DROP INDEX")
    } else if upper.starts_with("DROP SCHEMA") {
        Tag::new("DROP SCHEMA")
    } else if upper.starts_with("DROP") {
        Tag::new("DROP TABLE")
    } else if upper.starts_with("ALTER") {
        Tag::new("ALTER TABLE")
    } else if upper.starts_with("BEGIN") || upper.starts_with("START") {
        Tag::new("BEGIN")
    } else if upper.starts_with("COMMIT") {
        Tag::new("COMMIT")
    } else if upper.starts_with("ROLLBACK") {
        Tag::new("ROLLBACK")
    } else if upper.starts_with("SAVEPOINT") {
        Tag::new("SAVEPOINT")
    } else if upper.starts_with("RELEASE") {
        Tag::new("RELEASE")
    } else if upper.starts_with("SET") {
        Tag::new("SET")
    } else if upper.starts_with("COPY") {
        Tag::new("COPY").with_rows(affected_rows)
    } else {
        Tag::new("OK")
    }
}

fn pg_error_info(message: String, sqlstate: &str) -> ErrorInfo {
    ErrorInfo::new("ERROR".to_owned(), sqlstate.to_owned(), message)
}

fn parse_error_info(message: &str) -> ErrorInfo {
    pg_error_info(message.to_owned(), "42601")
}

fn invalid_parameter_error(message: String) -> ErrorInfo {
    pg_error_info(message, "22P02")
}

fn limbo_error_to_pg(error: LimboError) -> ErrorInfo {
    let message = error.to_string();
    let sqlstate = match &error {
        LimboError::ParseError(_)
        | LimboError::LexerError(_)
        | LimboError::ParseIntError(_)
        | LimboError::ParseFloatError(_) => classify_parse_sqlstate(&message),
        LimboError::PlanningError(_) => classify_planning_sqlstate(&message),
        LimboError::ForeignKeyConstraint(_) => "23503",
        LimboError::Constraint(msg) => classify_constraint_sqlstate(msg),
        LimboError::Busy | LimboError::BusySnapshot | LimboError::WriteWriteConflict => "40001",
        LimboError::ReadOnly => "25006",
        LimboError::TableLocked => "55P03",
        LimboError::Interrupt => "57014",
        LimboError::IntegerOverflow => "22003",
        LimboError::TooBig => "54000",
        LimboError::InvalidArgument(_)
        | LimboError::InvalidColumnType
        | LimboError::ConversionError(_)
        | LimboError::InvalidDate(_)
        | LimboError::InvalidTime(_)
        | LimboError::InvalidModifier(_)
        | LimboError::InvalidFormatter(_)
        | LimboError::NullValue
        | LimboError::InvalidBlobSize(_) => "22023",
        LimboError::DatabaseFull(_) => "53200",
        LimboError::Corrupt(_) | LimboError::NotADB => "XX001",
        LimboError::SchemaUpdated | LimboError::SchemaConflict => "40001",
        LimboError::ExtensionError(_) => "0A000",
        LimboError::Raise(_, _) => "P0001",
        LimboError::Conflict(_) => "40001",
        LimboError::TxError(_) | LimboError::TxTerminated | LimboError::NoSuchTransactionID(_) => {
            "25P02"
        }
        LimboError::InternalError(_)
        | LimboError::CheckpointFailed(_)
        | LimboError::Page1NotAlloc
        | LimboError::CommitDependencyAborted
        | LimboError::UnsupportedEncoding(_) => "XX000",
        LimboError::LockingError(_)
        | LimboError::CompletionError(_)
        | LimboError::CacheError(_) => classify_by_message(&message),
        LimboError::EnvVarError(_) | LimboError::RaiseIgnore => "XX000",
    };
    pg_error_info(message, sqlstate)
}

fn classify_parse_sqlstate(message: &str) -> &'static str {
    classify_by_message(message)
}

fn classify_planning_sqlstate(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("no such table") || lower.contains("table not found") {
        "42P01"
    } else if lower.contains("no such column") {
        "42703"
    } else if lower.contains("no such function") {
        "42883"
    } else if lower.contains("no such index") {
        "42704"
    } else {
        "42601"
    }
}

fn classify_constraint_sqlstate(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("unique") || lower.contains("primary key") {
        "23505"
    } else if lower.contains("not null") {
        "23502"
    } else if lower.contains("check") {
        "23514"
    } else if lower.contains("foreign key") {
        "23503"
    } else {
        "23000"
    }
}

fn classify_by_message(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("no such table") || lower.contains("table not found") {
        "42P01"
    } else if lower.contains("no such column") {
        "42703"
    } else if lower.contains("no such function") {
        "42883"
    } else if lower.contains("no such index") {
        "42704"
    } else if lower.contains("already exists") || lower.contains("duplicate") {
        "42P07"
    } else if lower.contains("syntax error") || lower.starts_with("parse error") {
        "42601"
    } else if lower.contains("database is busy") || lower.contains("write-write conflict") {
        "40001"
    } else if lower.contains("read-only") || lower.contains("readonly") {
        "25006"
    } else if lower.contains("interrupt") {
        "57014"
    } else {
        "XX000"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drop_schema_name_literal() {
        assert_eq!(
            drop_schema_name("DROP SCHEMA wireschema", &[]).as_deref(),
            Some("wireschema")
        );
        assert_eq!(
            drop_schema_name("drop schema if exists myschema cascade", &[]).as_deref(),
            Some("myschema")
        );
        assert_eq!(drop_schema_name("DROP SCHEMA public", &[]), None);
        assert_eq!(drop_schema_name("CREATE SCHEMA foo", &[]), None);
    }

    #[test]
    fn test_drop_schema_name_parameterized() {
        let param = "prep_schema".as_bytes();
        let bound = [Some(param)];
        assert_eq!(
            drop_schema_name("DROP SCHEMA $1", &bound).as_deref(),
            Some("prep_schema")
        );
        assert_eq!(
            drop_schema_name("DROP SCHEMA IF EXISTS $1 CASCADE", &bound).as_deref(),
            Some("prep_schema")
        );
    }

    #[test]
    fn test_drop_schema_name_missing_parameter() {
        assert_eq!(drop_schema_name("DROP SCHEMA $1", &[]), None);
    }

    #[test]
    fn test_limbo_error_sqlstate_mapping() {
        use turso_core::LimboError;

        assert_eq!(
            limbo_error_to_pg(LimboError::ParseError("syntax error near foo".into())).code,
            "42601"
        );
        assert_eq!(
            limbo_error_to_pg(LimboError::ParseError("no such table: missing".into())).code,
            "42P01"
        );
        assert_eq!(
            limbo_error_to_pg(LimboError::PlanningError("no such column: x".into())).code,
            "42703"
        );
        assert_eq!(
            limbo_error_to_pg(LimboError::ForeignKeyConstraint("fk failed".into())).code,
            "23503"
        );
        assert_eq!(
            limbo_error_to_pg(LimboError::Constraint(
                "UNIQUE constraint failed: t.c".into()
            ))
            .code,
            "23505"
        );
        assert_eq!(
            limbo_error_to_pg(LimboError::Constraint(
                "NOT NULL constraint failed: t.c".into()
            ))
            .code,
            "23502"
        );
        assert_eq!(limbo_error_to_pg(LimboError::Busy).code, "40001");
        assert_eq!(limbo_error_to_pg(LimboError::ReadOnly).code, "25006");
        assert_eq!(limbo_error_to_pg(LimboError::Interrupt).code, "57014");
        assert_eq!(invalid_parameter_error("bad param".into()).code, "22P02");
    }

    #[test]
    fn test_pg_bytes_to_value_integer() {
        let val = pg_bytes_to_value(b"42", &Type::INT4, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_i64(42));

        let val = pg_bytes_to_value(b"-100", &Type::INT8, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_i64(-100));

        let val = pg_bytes_to_value(b"0", &Type::INT2, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_i64(0));
    }

    #[test]
    fn test_pg_bytes_to_value_float() {
        let val = pg_bytes_to_value(b"3.25", &Type::FLOAT8, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_f64(3.25));

        let val = pg_bytes_to_value(b"-0.5", &Type::FLOAT4, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_f64(-0.5));

        let val = pg_bytes_to_value(b"1.23", &Type::NUMERIC, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_text("1.23"));
    }

    #[test]
    fn test_pg_bytes_to_value_bool() {
        let val = pg_bytes_to_value(b"t", &Type::BOOL, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_i64(1));

        let val = pg_bytes_to_value(b"f", &Type::BOOL, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_i64(0));

        let val = pg_bytes_to_value(b"true", &Type::BOOL, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_i64(1));

        let val = pg_bytes_to_value(b"false", &Type::BOOL, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_i64(0));
    }

    #[test]
    fn test_pg_bytes_to_value_text() {
        let val = pg_bytes_to_value(b"hello world", &Type::TEXT, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_text("hello world".to_owned()));

        let val = pg_bytes_to_value(b"Alice", &Type::VARCHAR, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_text("Alice".to_owned()));
    }

    #[test]
    fn test_pg_bytes_to_value_bytea() {
        let val = pg_bytes_to_value(b"\\xDEADBEEF", &Type::BYTEA, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_blob(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn test_pg_bytes_to_value_unknown_type_as_text() {
        // Unknown types should be treated as text
        let val = pg_bytes_to_value(b"some-uuid-value", &Type::UUID, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_text("some-uuid-value".to_owned()));
    }

    #[test]
    fn test_pg_bytes_to_value_integer_parse_error() {
        let result = pg_bytes_to_value(b"not_a_number", &Type::INT4, FieldFormat::Text);
        assert!(result.is_err());
    }

    #[test]
    fn test_pg_bytes_to_value_float_parse_error() {
        let result = pg_bytes_to_value(b"not_a_float", &Type::FLOAT8, FieldFormat::Text);
        assert!(result.is_err());
    }

    #[test]
    fn test_pg_bytes_to_value_bool_invalid() {
        let result = pg_bytes_to_value(b"maybe", &Type::BOOL, FieldFormat::Text);
        assert!(result.is_err());
    }

    #[test]
    fn test_pg_bytes_to_value_binary_int4() {
        let bytes = 42i32.to_be_bytes();
        let val = pg_bytes_to_value(&bytes, &Type::INT4, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_i64(42));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_int8() {
        let bytes = (-100i64).to_be_bytes();
        let val = pg_bytes_to_value(&bytes, &Type::INT8, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_i64(-100));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_bool() {
        let val = pg_bytes_to_value(&[1], &Type::BOOL, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_i64(1));

        let val = pg_bytes_to_value(&[0], &Type::BOOL, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_i64(0));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_float8() {
        let bytes = 3.25f64.to_be_bytes();
        let val = pg_bytes_to_value(&bytes, &Type::FLOAT8, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_f64(3.25));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_bytea() {
        let bytes = [0xDE, 0xAD, 0xBE, 0xEF];
        let val = pg_bytes_to_value(&bytes, &Type::BYTEA, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_blob(bytes.to_vec()));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_text_fallback() {
        let val = pg_bytes_to_value(b"hello", &Type::TEXT, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_text("hello".to_owned()));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_date() {
        let bytes = 0i32.to_be_bytes();
        let val = pg_bytes_to_value(&bytes, &Type::DATE, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_text("2000-01-01"));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_money() {
        let bytes = 1234i64.to_be_bytes();
        let val = pg_bytes_to_value(&bytes, &Type::MONEY, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_text("$12.34"));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_uuid() {
        let bytes = [
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ];
        let val = pg_bytes_to_value(&bytes, &Type::UUID, FieldFormat::Binary).unwrap();
        assert_eq!(
            val,
            Value::from_text("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn test_pg_bytes_to_value_binary_numeric() {
        let bytes = [0, 1, 0, 0, 0, 0, 0, 0, 0, 42];
        let val = pg_bytes_to_value(&bytes, &Type::NUMERIC, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_text("42"));
    }

    #[test]
    fn test_pg_bytes_to_value_text_numeric_preserves_precision() {
        let val = pg_bytes_to_value(b"12.340", &Type::NUMERIC, FieldFormat::Text).unwrap();
        assert_eq!(val, Value::from_text("12.340"));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_interval() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_500_000i64.to_be_bytes());
        bytes.extend_from_slice(&0i32.to_be_bytes());
        bytes.extend_from_slice(&0i32.to_be_bytes());
        let val = pg_bytes_to_value(&bytes, &Type::INTERVAL, FieldFormat::Binary).unwrap();
        assert_eq!(val, Value::from_text("1.5 secs"));
    }

    #[test]
    fn test_pg_bytes_to_value_binary_int4_invalid_length() {
        let result = pg_bytes_to_value(&[0, 1], &Type::INT4, FieldFormat::Binary);
        assert!(result.is_err());
        if let Err(PgWireError::UserError(info)) = result {
            assert_eq!(info.code, "22P02");
        } else {
            panic!("expected UserError with SQLSTATE 22P02");
        }
    }

    #[test]
    fn test_decode_hex() {
        assert_eq!(
            decode_hex("DEADBEEF").unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
        assert_eq!(decode_hex("00ff").unwrap(), vec![0x00, 0xFF]);
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
        assert!(decode_hex("0").is_err()); // odd length
        assert!(decode_hex("GG").is_err()); // invalid hex
    }

    #[test]
    fn test_sqlite_type_to_pg_type() {
        assert_eq!(sqlite_type_to_pg_type("INTEGER"), Type::INT4);
        assert_eq!(sqlite_type_to_pg_type("INT"), Type::INT4);
        assert_eq!(sqlite_type_to_pg_type("INT4"), Type::INT4);
        assert_eq!(sqlite_type_to_pg_type("SMALLINT"), Type::INT4);
        assert_eq!(sqlite_type_to_pg_type("BIGINT"), Type::INT8);
        assert_eq!(sqlite_type_to_pg_type("INT8"), Type::INT8);
        assert_eq!(sqlite_type_to_pg_type("REAL"), Type::FLOAT8);
        assert_eq!(sqlite_type_to_pg_type("TEXT"), Type::TEXT);
        assert_eq!(sqlite_type_to_pg_type("BLOB"), Type::BYTEA);
        assert_eq!(sqlite_type_to_pg_type("BOOLEAN"), Type::BOOL);
        assert_eq!(sqlite_type_to_pg_type("TIMESTAMP"), Type::TIMESTAMP);
        assert_eq!(sqlite_type_to_pg_type("TIMESTAMPTZ"), Type::TIMESTAMPTZ);
        assert_eq!(sqlite_type_to_pg_type("DATE"), Type::DATE);
        assert_eq!(sqlite_type_to_pg_type("JSON"), Type::JSON);
        assert_eq!(sqlite_type_to_pg_type("JSONB"), Type::JSONB);
        assert_eq!(sqlite_type_to_pg_type("UUID"), Type::UUID);
        assert_eq!(sqlite_type_to_pg_type("INTERVAL"), Type::INTERVAL);
        assert_eq!(sqlite_type_to_pg_type("MONEY"), Type::MONEY);
        // Unknown types map to TEXT
        assert_eq!(sqlite_type_to_pg_type("UNKNOWN"), Type::TEXT);
    }

    #[test]
    fn test_unknown_type_inference() {
        // UNKNOWN type should infer integers from numeric-looking strings
        let val = pg_bytes_to_value(b"42", &Type::UNKNOWN, FieldFormat::Text).unwrap();
        assert!(matches!(
            val,
            Value::Numeric(turso_core::Numeric::Integer(42))
        ));

        // UNKNOWN type should infer floats
        let val = pg_bytes_to_value(b"3.14", &Type::UNKNOWN, FieldFormat::Text).unwrap();
        if let Value::Numeric(turso_core::Numeric::Float(f)) = val {
            #[allow(clippy::approx_constant)]
            let expected = 3.14;
            assert!((f64::from(f) - expected).abs() < 0.001);
        } else {
            panic!("Expected Float");
        }

        // UNKNOWN type should keep text for non-numeric strings
        let val = pg_bytes_to_value(b"hello", &Type::UNKNOWN, FieldFormat::Text).unwrap();
        assert!(matches!(val, Value::Text(_)));
    }
}
