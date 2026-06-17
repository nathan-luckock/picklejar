//! HTTP/JSON API server for the picklejar engine.
//!
//! A single-threaded server (the engine is not `Send`) that owns one
//! `Database` and serves it over a tiny hand-written HTTP layer, so the
//! database and its API are both dependency-free. This is the backend the
//! studio UI talks to.
//!
//! Endpoints:
//! - `POST /api/query` with the SQL as the raw request body. Returns the
//!   result as JSON (rows, a mutation count, an EXPLAIN plan, a transaction
//!   message, or an error).
//! - `GET /api/tables` returns the schema (each table's columns and types).
//! - `GET /` is a health check.

mod http;
mod json;

use std::net::TcpListener;

use clap::Parser as ClapParser;
use picklejar::{DataType, Database, QueryOutcome, Value};

use crate::http::{read_request, write_response, Request};
use crate::json::Json;

#[derive(Debug, ClapParser)]
#[command(
    name = "picklejar-server",
    version,
    about = "HTTP API server for picklejar"
)]
struct Args {
    /// Path to the database file.
    #[arg(short, long, default_value = "picklejar.db")]
    database: String,
    /// TCP port to listen on.
    #[arg(short, long, default_value_t = 8080)]
    port: u16,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let mut db = match Database::open(&args.database) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("could not open {}: {e}", args.database);
            std::process::exit(1);
        }
    };

    let listener = match TcpListener::bind(("127.0.0.1", args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not bind port {}: {e}", args.port);
            std::process::exit(1);
        }
    };
    println!(
        "picklejar-server on http://127.0.0.1:{} (database {})",
        args.port, args.database
    );

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                continue;
            }
        };
        match read_request(&stream) {
            Ok(Some(req)) => {
                if let Err(e) = handle(&mut db, &req, &mut stream) {
                    tracing::warn!("write failed: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("read failed: {e}"),
        }
    }
}

/// Route one request and write its response.
fn handle(
    db: &mut Database,
    req: &Request,
    stream: &mut std::net::TcpStream,
) -> std::io::Result<()> {
    if req.method == "OPTIONS" {
        // CORS preflight.
        return write_response(stream, 204, "text/plain", "");
    }
    let (status, body) = match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/query") => (200, run_query(db, &req.body).render()),
        ("GET", "/api/tables") => (200, tables_json(db).render()),
        ("GET", "/" | "/health") => (200, obj(vec![("status", Json::Str("ok".into()))]).render()),
        _ => (
            404,
            obj(vec![("error", Json::Str("not found".into()))]).render(),
        ),
    };
    write_response(stream, status, "application/json", &body)
}

/// Run the SQL and encode the outcome (or error) as JSON.
fn run_query(db: &mut Database, sql: &str) -> Json {
    match db.execute(sql) {
        Ok(outcome) => outcome_json(outcome),
        Err(e) => obj(vec![
            ("type", Json::Str("error".into())),
            ("error", Json::Str(e.to_string())),
        ]),
    }
}

fn outcome_json(outcome: QueryOutcome) -> Json {
    match outcome {
        QueryOutcome::Ddl => obj(vec![("type", Json::Str("ok".into()))]),
        QueryOutcome::Mutation { affected } => obj(vec![
            ("type", Json::Str("mutation".into())),
            (
                "affected",
                Json::Int(i64::try_from(affected).unwrap_or(i64::MAX)),
            ),
        ]),
        QueryOutcome::Rows { columns, rows } => obj(vec![
            ("type", Json::Str("rows".into())),
            (
                "columns",
                Json::Array(columns.into_iter().map(Json::Str).collect()),
            ),
            (
                "rows",
                Json::Array(
                    rows.into_iter()
                        .map(|row| Json::Array(row.iter().map(value_json).collect()))
                        .collect(),
                ),
            ),
        ]),
        QueryOutcome::Explain(plan) => obj(vec![
            ("type", Json::Str("explain".into())),
            ("plan", Json::Str(plan)),
        ]),
        QueryOutcome::Message(text) => obj(vec![
            ("type", Json::Str("message".into())),
            ("text", Json::Str(text.to_string())),
        ]),
    }
}

fn value_json(v: &Value) -> Json {
    match v {
        Value::Int(n) => Json::Int(*n),
        Value::Float(x) => Json::Float(*x),
        // Text, and a JSON document as its raw text, both render as a string.
        Value::Text(s) | Value::Json(s) => Json::Str(s.clone()),
        Value::Bool(b) => Json::Bool(*b),
        // JSON has no date type; render the canonical string form.
        Value::Date(days) => Json::Str(picklejar_sql::datetime::format_date(*days)),
        Value::Timestamp(micros) => Json::Str(picklejar_sql::datetime::format_timestamp(*micros)),
        // A number rendered as a JSON number (it is exact base-10 text).
        Value::Decimal(m, s) => Json::Str(picklejar_sql::decimal::format(*m, *s)),
        Value::Null => Json::Null,
    }
}

/// The schema as JSON: each table's name and its columns with types.
fn tables_json(db: &Database) -> Json {
    let tables = db
        .table_names()
        .into_iter()
        .map(|name| {
            let columns = db
                .columns(&name)
                .unwrap_or_default()
                .into_iter()
                .map(|(cname, ty)| {
                    obj(vec![
                        ("name", Json::Str(cname)),
                        ("type", Json::Str(type_name(ty).into())),
                    ])
                })
                .collect();
            obj(vec![
                ("name", Json::Str(name)),
                ("columns", Json::Array(columns)),
            ])
        })
        .collect();
    obj(vec![("tables", Json::Array(tables))])
}

const fn type_name(ty: DataType) -> &'static str {
    match ty {
        DataType::Int => "INT",
        DataType::Float => "FLOAT",
        DataType::Bool => "BOOL",
        DataType::Text => "TEXT",
        DataType::Date => "DATE",
        DataType::Timestamp => "TIMESTAMP",
        DataType::Json => "JSON",
        DataType::Decimal => "DECIMAL",
    }
}

/// Build a JSON object from `(key, value)` pairs.
fn obj(fields: Vec<(&str, Json)>) -> Json {
    Json::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}
