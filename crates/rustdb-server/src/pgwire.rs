//! PostgreSQL v3 wire-protocol front end for the rustdb engine.
//!
//! This lets standard PostgreSQL clients (`psql`, drivers like psycopg and
//! JDBC, GUI tools) talk to rustdb over the real frontend/backend protocol.
//! One [`Database`] is served per process, one connection at a time, because
//! the engine is single-threaded (`!Send`).
//!
//! Implemented: the startup handshake (with SSL/GSS politely declined), trust
//! authentication, the **simple query protocol** (`Query` -> `RowDescription` /
//! `DataRow` / `CommandComplete` / `ReadyForQuery`, with `ErrorResponse` on
//! failure), and the **extended query protocol** (`Parse` / `Bind` /
//! `Describe` / `Execute` / `Close` / `Sync` / `Flush`) with positional `$N`
//! parameters, so server-side prepared statements and drivers that use them
//! work. Parameter values are bound by substituting them into the statement.
//! Text-format parameters are typed by their OID (an unspecified type is
//! inferred); the common binary scalar formats are also decoded. Result
//! columns are always sent in text format.
//!
//! Message framing follows the protocol exactly: every backend message is a
//! one-byte type tag, a big-endian `i32` length that counts itself but not the
//! tag, then the payload. Startup messages omit the tag.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use rustdb::{Database, QueryOutcome, Value};
use rustdb_sql::{Parser, Statement};

/// `int8` (64-bit integer) type OID.
const INT8_OID: i32 = 20;
/// `float8` (double precision) type OID.
const FLOAT8_OID: i32 = 701;
/// `bool` type OID.
const BOOL_OID: i32 = 16;
/// `text` type OID.
const TEXT_OID: i32 = 25;

/// Protocol version 3.0, sent in the startup message.
const PROTOCOL_3_0: i32 = 196_608;
/// Magic version asking to negotiate SSL.
const SSL_REQUEST: i32 = 80_877_103;
/// Magic version asking to negotiate GSSAPI encryption.
const GSS_REQUEST: i32 = 80_877_104;
/// Magic version for an out-of-band query cancel.
const CANCEL_REQUEST: i32 = 80_877_102;

/// Serve one client connection to completion: handshake, then a loop over
/// simple queries until the client terminates or the socket closes.
///
/// # Errors
///
/// Returns an I/O error if the socket cannot be read or written.
pub fn serve<S: Read + Write>(db: &mut Database, stream: &mut S) -> io::Result<()> {
    if startup(db, stream)? {
        query_loop(db, stream)?;
    }
    Ok(())
}

/// Run the startup handshake. Returns `true` once an authenticated 3.0 session
/// is ready for queries, or `false` if the client closed the connection or only
/// probed for SSL/cancel.
fn startup<S: Read + Write>(db: &Database, stream: &mut S) -> io::Result<bool> {
    loop {
        let Some(len) = read_i32_opt(stream)? else {
            return Ok(false);
        };
        if len < 8 {
            return Ok(false);
        }
        let mut payload = vec![0u8; usize::try_from(len).unwrap_or(0).saturating_sub(4)];
        stream.read_exact(&mut payload)?;
        let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        match code {
            // Decline encryption with a single byte, then read the real startup.
            SSL_REQUEST | GSS_REQUEST => {
                stream.write_all(b"N")?;
                stream.flush()?;
            }
            // A cancel request carries no session; acknowledge by closing.
            CANCEL_REQUEST => return Ok(false),
            PROTOCOL_3_0 => break,
            other => {
                send_error(stream, &format!("unsupported protocol version {other}"))?;
                return Ok(false);
            }
        }
    }

    // Trust authentication: AuthenticationOk.
    write_message(stream, b'R', &0i32.to_be_bytes())?;
    // A few parameters clients like to see at startup.
    for (key, val) in [
        ("server_version", "15.0 (rustdb)"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("integer_datetimes", "on"),
    ] {
        parameter_status(stream, key, val)?;
    }
    // BackendKeyData (a dummy pid/secret; we do not support cancel).
    let mut key_data = Vec::with_capacity(8);
    key_data.extend_from_slice(&1234i32.to_be_bytes());
    key_data.extend_from_slice(&5678i32.to_be_bytes());
    write_message(stream, b'K', &key_data)?;
    ready_for_query(db, stream)?;
    Ok(true)
}

/// A prepared statement: its raw SQL and the parameter type OIDs from `Parse`.
struct Prepared {
    sql: String,
    param_oids: Vec<i32>,
}

/// A bound portal: the parameter-substituted SQL, whether it returns rows, and
/// a result cached by a preceding `Describe` so `Execute` does not re-run it.
struct Portal {
    sql: String,
    row_returning: bool,
    cached: Option<QueryOutcome>,
}

/// Per-connection extended-query state.
#[derive(Default)]
struct Session {
    prepared: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// Set when an extended-query step errors; the rest of the batch is ignored
    /// until the next `Sync`, as the protocol requires.
    failed: bool,
}

/// Read and answer frontend messages until termination or EOF. Handles both the
/// simple query protocol (`Query`) and the extended one (`Parse` / `Bind` /
/// `Describe` / `Execute` / `Close` / `Sync` / `Flush`).
fn query_loop<S: Read + Write>(db: &mut Database, stream: &mut S) -> io::Result<()> {
    let mut session = Session::default();
    loop {
        let mut tag = [0u8; 1];
        if !read_exact_opt(stream, &mut tag)? {
            return Ok(()); // clean EOF
        }
        let len = read_i32(stream)?;
        let body_len = usize::try_from(len).unwrap_or(0).saturating_sub(4);
        let mut payload = vec![0u8; body_len];
        stream.read_exact(&mut payload)?;

        match tag[0] {
            b'Q' => {
                session.failed = false;
                handle_query(db, stream, &payload)?;
            }
            b'P' => handle_parse(&mut session, stream, &payload)?,
            b'B' => handle_bind(&mut session, stream, &payload)?,
            b'D' => handle_describe(&mut session, db, stream, &payload)?,
            b'E' => handle_execute(&mut session, db, stream, &payload)?,
            b'C' => handle_close(&mut session, stream, &payload)?,
            b'H' => stream.flush()?, // Flush
            b'S' => {
                // Sync ends the extended-query block and resyncs.
                session.failed = false;
                ready_for_query(db, stream)?;
            }
            b'X' => return Ok(()), // Terminate
            other => {
                if !session.failed {
                    send_error(
                        stream,
                        &format!("unsupported frontend message '{}'", other as char),
                    )?;
                    session.failed = true;
                }
            }
        }
    }
}

/// `Parse`: record a prepared statement (validating that it parses).
fn handle_parse<S: Write>(session: &mut Session, stream: &mut S, payload: &[u8]) -> io::Result<()> {
    if session.failed {
        return Ok(());
    }
    let mut off = 0;
    let name = read_cstr(payload, &mut off);
    let query = read_cstr(payload, &mut off);
    let nparams = read_i16(payload, &mut off);
    let param_oids: Vec<i32> = (0..nparams.max(0))
        .map(|_| read_i32p(payload, &mut off))
        .collect();
    match parse_one(&query) {
        Ok(_) => {
            session.prepared.insert(
                name,
                Prepared {
                    sql: query,
                    param_oids,
                },
            );
            write_message(stream, b'1', &[]) // ParseComplete
        }
        Err(e) => fail(session, stream, &e),
    }
}

/// `Bind`: substitute parameter values into a prepared statement, creating a
/// portal.
fn handle_bind<S: Write>(session: &mut Session, stream: &mut S, payload: &[u8]) -> io::Result<()> {
    if session.failed {
        return Ok(());
    }
    let mut off = 0;
    let portal_name = read_cstr(payload, &mut off);
    let stmt_name = read_cstr(payload, &mut off);
    let n_fmt = read_i16(payload, &mut off);
    let formats: Vec<i16> = (0..n_fmt.max(0))
        .map(|_| read_i16(payload, &mut off))
        .collect();
    let n_vals = read_i16(payload, &mut off);
    let mut raw: Vec<Option<Vec<u8>>> = Vec::new();
    for _ in 0..n_vals.max(0) {
        let len = read_i32p(payload, &mut off);
        if len < 0 {
            raw.push(None);
        } else {
            raw.push(Some(read_bytes(
                payload,
                &mut off,
                usize::try_from(len).unwrap_or(0),
            )));
        }
    }
    // Result format codes follow; we always send text, so skip them.
    let n_res = read_i16(payload, &mut off);
    for _ in 0..n_res.max(0) {
        read_i16(payload, &mut off);
    }

    let Some((sql, oids)) = session
        .prepared
        .get(&stmt_name)
        .map(|p| (p.sql.clone(), p.param_oids.clone()))
    else {
        return fail(
            session,
            stream,
            &format!("unknown prepared statement \"{stmt_name}\""),
        );
    };

    let mut values = Vec::with_capacity(raw.len());
    for (i, bytes) in raw.iter().enumerate() {
        let fmt = param_format(&formats, i);
        let oid = oids.get(i).copied().unwrap_or(0);
        match decode_param(bytes.as_deref(), fmt, oid) {
            Ok(v) => values.push(v),
            Err(msg) => return fail(session, stream, &msg),
        }
    }

    let parsed = match parse_one(&sql) {
        Ok(s) => s,
        Err(e) => return fail(session, stream, &e),
    };
    let bound = parsed.substitute_params(&values);
    let row_returning = matches!(
        bound,
        Statement::Select(_) | Statement::Union { .. } | Statement::Explain(_)
    );
    session.portals.insert(
        portal_name,
        Portal {
            sql: bound.to_string(),
            row_returning,
            cached: None,
        },
    );
    write_message(stream, b'2', &[]) // BindComplete
}

/// `Describe`: report the result shape of a statement or portal.
fn handle_describe<S: Write>(
    session: &mut Session,
    db: &mut Database,
    stream: &mut S,
    payload: &[u8],
) -> io::Result<()> {
    if session.failed {
        return Ok(());
    }
    let mut off = 0;
    let kind = payload.first().copied().unwrap_or(b'P');
    off += 1;
    let name = read_cstr(payload, &mut off);
    if kind == b'S' {
        // Statement: ParameterDescription, then NoData (result columns of an
        // unbound statement are not computed).
        let oids = session
            .prepared
            .get(&name)
            .map(|p| p.param_oids.clone())
            .unwrap_or_default();
        let mut pd = Vec::new();
        pd.extend_from_slice(&i16::try_from(oids.len()).unwrap_or(0).to_be_bytes());
        for o in &oids {
            pd.extend_from_slice(&o.to_be_bytes());
        }
        write_message(stream, b't', &pd)?;
        write_message(stream, b'n', &[]) // NoData
    } else {
        // Portal: run a row-returning statement now to learn its columns, and
        // cache the result for the following Execute.
        let Some(portal) = session.portals.get_mut(&name) else {
            return fail(session, stream, &format!("unknown portal \"{name}\""));
        };
        if !portal.row_returning {
            return write_message(stream, b'n', &[]); // NoData
        }
        match db.execute(&portal.sql) {
            Ok(outcome) => {
                if let Some((columns, rows)) = outcome_as_rows(&outcome) {
                    row_description(stream, &columns, &rows)?;
                } else {
                    write_message(stream, b'n', &[])?;
                }
                portal.cached = Some(outcome);
                Ok(())
            }
            Err(e) => fail(session, stream, &e.to_string()),
        }
    }
}

/// `Execute`: run a portal and stream its rows (or its command tag).
fn handle_execute<S: Write>(
    session: &mut Session,
    db: &mut Database,
    stream: &mut S,
    payload: &[u8],
) -> io::Result<()> {
    if session.failed {
        return Ok(());
    }
    let mut off = 0;
    let name = read_cstr(payload, &mut off);
    let _max_rows = read_i32p(payload, &mut off);

    let Some(portal) = session.portals.get_mut(&name) else {
        return fail(session, stream, &format!("unknown portal \"{name}\""));
    };
    let outcome = match portal.cached.take() {
        Some(o) => o,
        None => match db.execute(&portal.sql) {
            Ok(o) => o,
            Err(e) => return fail(session, stream, &e.to_string()),
        },
    };
    if let Some((_columns, rows)) = outcome_as_rows(&outcome) {
        // Execute does not repeat RowDescription (Describe sent it).
        for row in &rows {
            data_row(stream, row)?;
        }
        let tag = if matches!(outcome, QueryOutcome::Explain(_)) {
            "EXPLAIN".to_string()
        } else {
            format!("SELECT {}", rows.len())
        };
        command_complete(stream, &tag)
    } else {
        match &outcome {
            QueryOutcome::Mutation { affected } => {
                command_complete(stream, &mutation_tag(&portal.sql, *affected))
            }
            QueryOutcome::Message(text) => command_complete(stream, text),
            _ => command_complete(stream, ddl_tag(&portal.sql)),
        }
    }
}

/// `Close`: drop a prepared statement or portal.
fn handle_close<S: Write>(session: &mut Session, stream: &mut S, payload: &[u8]) -> io::Result<()> {
    if session.failed {
        return Ok(());
    }
    let mut off = 0;
    let kind = payload.first().copied().unwrap_or(b'P');
    off += 1;
    let name = read_cstr(payload, &mut off);
    match kind {
        b'S' => {
            session.prepared.remove(&name);
        }
        _ => {
            session.portals.remove(&name);
        }
    }
    write_message(stream, b'3', &[]) // CloseComplete
}

/// Report an error in extended-query mode: send `ErrorResponse` and enter the
/// failed state, where the rest of the batch is ignored until `Sync`.
fn fail<S: Write>(session: &mut Session, stream: &mut S, message: &str) -> io::Result<()> {
    session.failed = true;
    send_error(stream, message)
}

/// Parse a single SQL statement.
fn parse_one(sql: &str) -> std::result::Result<Statement, String> {
    Parser::from_sql(sql)
        .and_then(|mut p| p.parse_statement())
        .map_err(|e| e.to_string())
}

/// The `(columns, rows)` of a row-returning outcome (`SELECT` or `EXPLAIN`), or
/// `None` for a statement that returns no rows.
fn outcome_as_rows(outcome: &QueryOutcome) -> Option<(Vec<String>, Vec<Vec<Value>>)> {
    match outcome {
        QueryOutcome::Rows { columns, rows } => Some((columns.clone(), rows.clone())),
        QueryOutcome::Explain(plan) => Some((
            vec!["QUERY PLAN".to_string()],
            plan.lines()
                .map(|line| vec![Value::Text(line.to_string())])
                .collect(),
        )),
        _ => None,
    }
}

/// The format code for parameter `i`: text (0) if none given, the single code if
/// one applies to all, otherwise the per-parameter code.
fn param_format(formats: &[i16], i: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(i).copied().unwrap_or(0),
    }
}

/// Decode one bound parameter to a value, honoring its format code and type OID.
fn decode_param(bytes: Option<&[u8]>, fmt: i16, oid: i32) -> std::result::Result<Value, String> {
    let Some(bytes) = bytes else {
        return Ok(Value::Null);
    };
    if fmt == 1 {
        return decode_binary_param(bytes, oid);
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| "parameter is not valid UTF-8".to_string())?;
    Ok(value_from_text(text, oid))
}

/// A text-format parameter as a value, typed by its OID. An unspecified type
/// (OID 0) is inferred: integer, then float, else text.
fn value_from_text(s: &str, oid: i32) -> Value {
    match oid {
        20 | 21 | 23 => s
            .parse::<i64>()
            .map_or_else(|_| Value::Text(s.to_string()), Value::Int),
        700 | 701 => s
            .parse::<f64>()
            .map_or_else(|_| Value::Text(s.to_string()), Value::Float),
        16 => Value::Bool(matches!(s, "t" | "true" | "TRUE" | "1")),
        25 | 1043 => Value::Text(s.to_string()),
        _ => s
            .parse::<i64>()
            .map(Value::Int)
            .or_else(|_| s.parse::<f64>().map(Value::Float))
            .unwrap_or_else(|_| Value::Text(s.to_string())),
    }
}

/// Decode a binary-format parameter for the common scalar types.
fn decode_binary_param(bytes: &[u8], oid: i32) -> std::result::Result<Value, String> {
    let int_be = |b: &[u8]| -> Option<i64> {
        match b.len() {
            2 => Some(i64::from(i16::from_be_bytes([b[0], b[1]]))),
            4 => Some(i64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))),
            8 => Some(i64::from_be_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ])),
            _ => None,
        }
    };
    match oid {
        20 | 21 | 23 => int_be(bytes)
            .map(Value::Int)
            .ok_or_else(|| "malformed binary integer parameter".to_string()),
        701 if bytes.len() == 8 => Ok(Value::Float(f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))),
        700 if bytes.len() == 4 => Ok(Value::Float(f64::from(f32::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ])))),
        16 if bytes.len() == 1 => Ok(Value::Bool(bytes[0] != 0)),
        25 | 1043 | 0 => std::str::from_utf8(bytes)
            .map(|s| Value::Text(s.to_string()))
            .map_err(|_| "binary text parameter is not valid UTF-8".to_string()),
        other => Err(format!("unsupported binary parameter type OID {other}")),
    }
}

/// Read a NUL-terminated string from `payload` at `*off`, advancing past it.
fn read_cstr(payload: &[u8], off: &mut usize) -> String {
    let start = *off;
    while *off < payload.len() && payload[*off] != 0 {
        *off += 1;
    }
    let s = String::from_utf8_lossy(&payload[start..*off]).into_owned();
    if *off < payload.len() {
        *off += 1; // skip the NUL
    }
    s
}

/// Read a big-endian `i16` from `payload` at `*off` (0 if truncated).
fn read_i16(payload: &[u8], off: &mut usize) -> i16 {
    let v = payload
        .get(*off..*off + 2)
        .map_or(0, |b| i16::from_be_bytes([b[0], b[1]]));
    *off += 2;
    v
}

/// Read a big-endian `i32` from `payload` at `*off` (0 if truncated).
fn read_i32p(payload: &[u8], off: &mut usize) -> i32 {
    let v = payload
        .get(*off..*off + 4)
        .map_or(0, |b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    *off += 4;
    v
}

/// Read `len` bytes from `payload` at `*off`, advancing past them.
fn read_bytes(payload: &[u8], off: &mut usize, len: usize) -> Vec<u8> {
    let end = (*off + len).min(payload.len());
    let v = payload[*off..end].to_vec();
    *off = end;
    v
}

/// Execute one simple-query string (which may hold several `;`-separated
/// statements) and write the responses, ending with `ReadyForQuery`.
fn handle_query<S: Write>(db: &mut Database, stream: &mut S, payload: &[u8]) -> io::Result<()> {
    let sql = cstr(payload);
    let statements = split_statements(&sql);
    if statements.is_empty() {
        write_message(stream, b'I', &[])?; // EmptyQueryResponse
        return ready_for_query(db, stream);
    }
    for statement in statements {
        match db.execute(&statement) {
            Ok(outcome) => respond(stream, &statement, outcome)?,
            Err(e) => {
                // Postgres reports the error and abandons the rest of the batch.
                send_error(stream, &e.to_string())?;
                break;
            }
        }
    }
    ready_for_query(db, stream)
}

/// Write the backend messages for one statement's outcome.
fn respond<S: Write>(stream: &mut S, sql: &str, outcome: QueryOutcome) -> io::Result<()> {
    match outcome {
        QueryOutcome::Rows { columns, rows } => {
            row_description(stream, &columns, &rows)?;
            for row in &rows {
                data_row(stream, row)?;
            }
            command_complete(stream, &format!("SELECT {}", rows.len()))
        }
        QueryOutcome::Explain(plan) => {
            // Present a plan the way Postgres does: one text column, a row per
            // line.
            let columns = vec!["QUERY PLAN".to_string()];
            let rows: Vec<Vec<Value>> = plan
                .lines()
                .map(|line| vec![Value::Text(line.to_string())])
                .collect();
            row_description(stream, &columns, &rows)?;
            for row in &rows {
                data_row(stream, row)?;
            }
            command_complete(stream, "EXPLAIN")
        }
        QueryOutcome::Mutation { affected } => {
            command_complete(stream, &mutation_tag(sql, affected))
        }
        QueryOutcome::Ddl => command_complete(stream, ddl_tag(sql)),
        QueryOutcome::Message(text) => command_complete(stream, text),
    }
}

/// Send a `RowDescription` for `columns`, inferring each column's type OID from
/// the first non-null value in the result.
fn row_description<S: Write>(
    stream: &mut S,
    columns: &[String],
    rows: &[Vec<Value>],
) -> io::Result<()> {
    let count = i16::try_from(columns.len()).unwrap_or(i16::MAX);
    let mut payload = Vec::new();
    payload.extend_from_slice(&count.to_be_bytes());
    for (i, name) in columns.iter().enumerate() {
        let oid = column_oid(rows, i);
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload.extend_from_slice(&0i32.to_be_bytes()); // table OID
        payload.extend_from_slice(&0i16.to_be_bytes()); // column attribute number
        payload.extend_from_slice(&oid.to_be_bytes()); // type OID
        payload.extend_from_slice(&type_size(oid).to_be_bytes()); // type size
        payload.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        payload.extend_from_slice(&0i16.to_be_bytes()); // text format
    }
    write_message(stream, b'T', &payload)
}

/// Send one `DataRow` with each value in its text representation.
fn data_row<S: Write>(stream: &mut S, row: &[Value]) -> io::Result<()> {
    let count = i16::try_from(row.len()).unwrap_or(i16::MAX);
    let mut payload = Vec::new();
    payload.extend_from_slice(&count.to_be_bytes());
    for value in row {
        match value_text(value) {
            None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                let n = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
                payload.extend_from_slice(&n.to_be_bytes());
                payload.extend_from_slice(&bytes);
            }
        }
    }
    write_message(stream, b'D', &payload)
}

/// Send a `CommandComplete` with the given tag.
fn command_complete<S: Write>(stream: &mut S, tag: &str) -> io::Result<()> {
    let mut payload = Vec::with_capacity(tag.len() + 1);
    payload.extend_from_slice(tag.as_bytes());
    payload.push(0);
    write_message(stream, b'C', &payload)
}

/// Send `ReadyForQuery` with the current transaction status.
fn ready_for_query<S: Write>(db: &Database, stream: &mut S) -> io::Result<()> {
    let status = if db.in_transaction() { b'T' } else { b'I' };
    write_message(stream, b'Z', &[status])
}

/// Send an `ErrorResponse` carrying severity, a generic SQLSTATE, and `message`.
fn send_error<S: Write>(stream: &mut S, message: &str) -> io::Result<()> {
    let mut payload = Vec::new();
    for (field, value) in [(b'S', "ERROR"), (b'C', "XX000"), (b'M', message)] {
        payload.push(field);
        payload.extend_from_slice(value.as_bytes());
        payload.push(0);
    }
    payload.push(0); // terminator
    write_message(stream, b'E', &payload)
}

/// Send a `ParameterStatus` (`key` = `value`).
fn parameter_status<S: Write>(stream: &mut S, key: &str, value: &str) -> io::Result<()> {
    let mut payload = Vec::new();
    payload.extend_from_slice(key.as_bytes());
    payload.push(0);
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    write_message(stream, b'S', &payload)
}

/// Frame and write one backend message: tag, length (counting itself), payload.
fn write_message<S: Write>(stream: &mut S, tag: u8, payload: &[u8]) -> io::Result<()> {
    let len = i32::try_from(payload.len() + 4).unwrap_or(i32::MAX);
    stream.write_all(&[tag])?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

/// The Postgres text representation of a value, or `None` for SQL NULL.
fn value_text(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::Int(n) => Some(n.to_string().into_bytes()),
        Value::Float(x) => Some(x.to_string().into_bytes()),
        // Postgres renders booleans as `t` / `f` in text format.
        Value::Bool(b) => Some(if *b { b"t".to_vec() } else { b"f".to_vec() }),
        Value::Text(s) => Some(s.clone().into_bytes()),
    }
}

/// Infer a column's type OID from the first non-null value in it; default
/// `text` if the column is empty or all-null.
fn column_oid(rows: &[Vec<Value>], col: usize) -> i32 {
    for row in rows {
        match row.get(col) {
            Some(Value::Int(_)) => return INT8_OID,
            Some(Value::Float(_)) => return FLOAT8_OID,
            Some(Value::Bool(_)) => return BOOL_OID,
            Some(Value::Text(_)) => return TEXT_OID,
            _ => {}
        }
    }
    TEXT_OID
}

/// The fixed byte width of a type, or `-1` for variable-length.
const fn type_size(oid: i32) -> i16 {
    match oid {
        INT8_OID | FLOAT8_OID => 8,
        BOOL_OID => 1,
        _ => -1,
    }
}

/// The `CommandComplete` tag for a DML statement.
fn mutation_tag(sql: &str, affected: usize) -> String {
    match leading_word(sql).to_ascii_uppercase().as_str() {
        "INSERT" => format!("INSERT 0 {affected}"),
        "DELETE" => format!("DELETE {affected}"),
        _ => format!("UPDATE {affected}"),
    }
}

/// The `CommandComplete` tag for a DDL statement.
fn ddl_tag(sql: &str) -> &'static str {
    let mut words = sql.split_whitespace();
    let first = words.next().unwrap_or("").to_ascii_uppercase();
    let second = words.next().unwrap_or("").to_ascii_uppercase();
    match (first.as_str(), second.as_str()) {
        ("CREATE", "TABLE") => "CREATE TABLE",
        ("CREATE", "INDEX") => "CREATE INDEX",
        ("CREATE", "VIEW") => "CREATE VIEW",
        ("DROP", "TABLE") => "DROP TABLE",
        ("DROP", "VIEW") => "DROP VIEW",
        ("ALTER", "TABLE") => "ALTER TABLE",
        ("TRUNCATE", _) => "TRUNCATE TABLE",
        _ => "OK",
    }
}

/// The first whitespace-delimited word of `sql`.
fn leading_word(sql: &str) -> &str {
    sql.split_whitespace().next().unwrap_or("")
}

/// Decode a null-terminated C string from the front of `payload`.
fn cstr(payload: &[u8]) -> String {
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).into_owned()
}

/// Split a simple-query string into individual statements on `;`, ignoring
/// semicolons inside single-quoted string literals. Blank fragments are
/// dropped.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in sql.chars() {
        match ch {
            '\'' => {
                in_quote = !in_quote;
                current.push(ch);
            }
            ';' if !in_quote => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Read a big-endian `i32`, returning `None` if the stream is cleanly at EOF
/// before any byte is read.
fn read_i32_opt<S: Read>(stream: &mut S) -> io::Result<Option<i32>> {
    let mut buf = [0u8; 4];
    if read_exact_opt(stream, &mut buf)? {
        Ok(Some(i32::from_be_bytes(buf)))
    } else {
        Ok(None)
    }
}

/// Read a big-endian `i32`, erroring on EOF.
fn read_i32<S: Read>(stream: &mut S) -> io::Result<i32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    Ok(i32::from_be_bytes(buf))
}

/// Fill `buf`, returning `false` if EOF arrives before the first byte (a clean
/// close) and erroring on EOF partway through.
fn read_exact_opt<S: Read>(stream: &mut S, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..])? {
            0 => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-message",
                ));
            }
            n => filled += n,
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    /// An in-memory bidirectional stream: reads come from a pre-scripted input
    /// buffer, writes accumulate in `output`. The simple query protocol is
    /// strictly request/response, so a fully scripted input drives a whole
    /// session deterministically without a socket.
    struct MockStream {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Append a startup message (protocol 3.0 with a `user` parameter).
    fn push_startup(buf: &mut Vec<u8>) {
        let mut body = Vec::new();
        body.extend_from_slice(&PROTOCOL_3_0.to_be_bytes());
        body.extend_from_slice(b"user\0postgres\0\0");
        let len = i32::try_from(body.len() + 4).unwrap();
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&body);
    }

    /// Append a simple `Query` message.
    fn push_query(buf: &mut Vec<u8>, sql: &str) {
        let mut body = Vec::new();
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        let len = i32::try_from(body.len() + 4).unwrap();
        buf.push(b'Q');
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&body);
    }

    /// Append a `Terminate` message.
    fn push_terminate(buf: &mut Vec<u8>) {
        buf.push(b'X');
        buf.extend_from_slice(&4i32.to_be_bytes());
    }

    /// Frame a frontend message (tag + length + body) into `buf`.
    fn push_msg(buf: &mut Vec<u8>, tag: u8, body: &[u8]) {
        buf.push(tag);
        buf.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        buf.extend_from_slice(body);
    }

    fn push_parse(buf: &mut Vec<u8>, name: &str, query: &str, oids: &[i32]) {
        let mut body = Vec::new();
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(query.as_bytes());
        body.push(0);
        body.extend_from_slice(&i16::try_from(oids.len()).unwrap().to_be_bytes());
        for o in oids {
            body.extend_from_slice(&o.to_be_bytes());
        }
        push_msg(buf, b'P', &body);
    }

    /// Bind with all-text parameters and text result format.
    fn push_bind(buf: &mut Vec<u8>, portal: &str, stmt: &str, params: &[Option<&str>]) {
        let mut body = Vec::new();
        body.extend_from_slice(portal.as_bytes());
        body.push(0);
        body.extend_from_slice(stmt.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i16.to_be_bytes()); // no param format codes => all text
        body.extend_from_slice(&i16::try_from(params.len()).unwrap().to_be_bytes());
        for p in params {
            match p {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(s) => {
                    body.extend_from_slice(&i32::try_from(s.len()).unwrap().to_be_bytes());
                    body.extend_from_slice(s.as_bytes());
                }
            }
        }
        body.extend_from_slice(&0i16.to_be_bytes()); // no result format codes => text
        push_msg(buf, b'B', &body);
    }

    fn push_describe(buf: &mut Vec<u8>, kind: u8, name: &str) {
        let mut body = vec![kind];
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        push_msg(buf, b'D', &body);
    }

    fn push_execute(buf: &mut Vec<u8>, portal: &str) {
        let mut body = Vec::new();
        body.extend_from_slice(portal.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // max rows: all
        push_msg(buf, b'E', &body);
    }

    fn push_sync(buf: &mut Vec<u8>) {
        push_msg(buf, b'S', &[]);
    }

    /// Split a backend byte stream into `(tag, payload)` messages.
    fn parse_backend(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 5 <= bytes.len() {
            let tag = bytes[i];
            let len = u32::from_be_bytes([bytes[i + 1], bytes[i + 2], bytes[i + 3], bytes[i + 4]])
                as usize;
            let payload = bytes[i + 5..i + 1 + len].to_vec();
            out.push((tag, payload));
            i += 1 + len;
        }
        out
    }

    fn run(script: Vec<u8>) -> Vec<(u8, Vec<u8>)> {
        let dir = TempDir::new().unwrap();
        let mut db = Database::open(dir.path().join("pg.db")).unwrap();
        let mut stream = MockStream {
            input: Cursor::new(script),
            output: Vec::new(),
        };
        serve(&mut db, &mut stream).unwrap();
        parse_backend(&stream.output)
    }

    #[test]
    fn handshake_sends_auth_and_ready() {
        let mut script = Vec::new();
        push_startup(&mut script);
        push_terminate(&mut script);
        let msgs = run(script);
        // AuthenticationOk, then a ReadyForQuery.
        assert!(msgs
            .iter()
            .any(|(t, p)| *t == b'R' && p == &0i32.to_be_bytes()));
        assert!(msgs.iter().any(|(t, p)| *t == b'Z' && p == b"I"));
    }

    #[test]
    fn select_round_trips_rows() {
        let mut script = Vec::new();
        push_startup(&mut script);
        push_query(
            &mut script,
            "CREATE TABLE t (id INT, flag BOOL, label TEXT)",
        );
        push_query(&mut script, "INSERT INTO t VALUES (42, TRUE, 'hi')");
        push_query(&mut script, "SELECT id, flag, label FROM t");
        push_terminate(&mut script);
        let msgs = run(script);

        // The CREATE and INSERT command tags.
        assert!(msgs
            .iter()
            .any(|(t, p)| *t == b'C' && p.starts_with(b"CREATE TABLE")));
        assert!(msgs
            .iter()
            .any(|(t, p)| *t == b'C' && p.starts_with(b"INSERT 0 1")));

        // The SELECT produced a RowDescription naming the three columns.
        let row_desc = msgs
            .iter()
            .find(|(t, _)| *t == b'T')
            .expect("RowDescription");
        let desc = String::from_utf8_lossy(&row_desc.1);
        assert!(desc.contains("id") && desc.contains("flag") && desc.contains("label"));

        // The DataRow carries the text encodings, with bool rendered `t`.
        let data = msgs.iter().find(|(t, _)| *t == b'D').expect("DataRow");
        let body = String::from_utf8_lossy(&data.1);
        assert!(body.contains("42"), "row was {body:?}");
        assert!(body.contains('t'), "bool should encode as t: {body:?}");
        assert!(body.contains("hi"), "row was {body:?}");

        // And a SELECT 1 completion.
        assert!(msgs
            .iter()
            .any(|(t, p)| *t == b'C' && p.starts_with(b"SELECT 1")));
    }

    #[test]
    fn error_is_reported_then_resyncs() {
        let mut script = Vec::new();
        push_startup(&mut script);
        push_query(&mut script, "SELECT * FROM ghost");
        push_terminate(&mut script);
        let msgs = run(script);
        // An ErrorResponse, followed (eventually) by ReadyForQuery.
        let err = msgs
            .iter()
            .position(|(t, _)| *t == b'E')
            .expect("ErrorResponse");
        let ready = msgs
            .iter()
            .skip(err)
            .position(|(t, _)| *t == b'Z')
            .expect("ReadyForQuery after error");
        assert!(ready >= 1);
    }

    #[test]
    fn ddl_and_dml_tags() {
        assert_eq!(ddl_tag("CREATE TABLE t (id INT)"), "CREATE TABLE");
        assert_eq!(ddl_tag("create view v as select 1"), "CREATE VIEW");
        assert_eq!(ddl_tag("DROP TABLE t"), "DROP TABLE");
        assert_eq!(mutation_tag("INSERT INTO t VALUES (1)", 3), "INSERT 0 3");
        assert_eq!(mutation_tag("UPDATE t SET x = 1", 2), "UPDATE 2");
        assert_eq!(mutation_tag("DELETE FROM t", 5), "DELETE 5");
    }

    #[test]
    fn split_respects_quoted_semicolons() {
        let parts = split_statements("INSERT INTO t VALUES ('a;b'); SELECT 1");
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("'a;b'"));
        assert_eq!(parts[1], "SELECT 1");
    }

    #[test]
    fn extended_protocol_parameterized_select() {
        let mut script = Vec::new();
        push_startup(&mut script);
        push_query(&mut script, "CREATE TABLE t (id INT, name TEXT)");
        push_query(&mut script, "INSERT INTO t VALUES (1, 'alice'), (2, 'bob')");
        // Prepare, bind $1 = 2 (int8), describe, execute, sync.
        push_parse(&mut script, "", "SELECT name FROM t WHERE id = $1", &[20]);
        push_bind(&mut script, "", "", &[Some("2")]);
        push_describe(&mut script, b'P', "");
        push_execute(&mut script, "");
        push_sync(&mut script);
        push_terminate(&mut script);
        let msgs = run(script);

        assert!(msgs.iter().any(|(t, _)| *t == b'1'), "ParseComplete");
        assert!(msgs.iter().any(|(t, _)| *t == b'2'), "BindComplete");
        // The bound query returned exactly bob.
        let data = msgs.iter().find(|(t, _)| *t == b'D').expect("DataRow");
        assert!(
            String::from_utf8_lossy(&data.1).contains("bob"),
            "row was {:?}",
            data.1
        );
        assert!(msgs
            .iter()
            .any(|(t, p)| *t == b'C' && p.starts_with(b"SELECT 1")));
    }

    #[test]
    fn extended_protocol_parameterized_insert() {
        let mut script = Vec::new();
        push_startup(&mut script);
        push_query(&mut script, "CREATE TABLE t (id INT, name TEXT)");
        // A parameterized INSERT, executed without a Describe.
        push_parse(
            &mut script,
            "ins",
            "INSERT INTO t VALUES ($1, $2)",
            &[20, 25],
        );
        push_bind(&mut script, "", "ins", &[Some("7"), Some("carol")]);
        push_execute(&mut script, "");
        push_sync(&mut script);
        // A following simple query observes the inserted row.
        push_query(&mut script, "SELECT name FROM t WHERE id = 7");
        push_terminate(&mut script);
        let msgs = run(script);

        assert!(msgs
            .iter()
            .any(|(t, p)| *t == b'C' && p.starts_with(b"INSERT 0 1")));
        assert!(
            msgs.iter()
                .any(|(t, p)| *t == b'D' && String::from_utf8_lossy(p).contains("carol")),
            "the inserted row should be visible",
        );
    }

    #[test]
    fn extended_protocol_null_parameter() {
        let mut script = Vec::new();
        push_startup(&mut script);
        push_query(&mut script, "CREATE TABLE t (id INT, name TEXT)");
        push_query(&mut script, "INSERT INTO t VALUES (1, 'alice')");
        // $1 = NULL: `name = NULL` is unknown, so no rows match.
        push_parse(&mut script, "", "SELECT name FROM t WHERE name = $1", &[25]);
        push_bind(&mut script, "", "", &[None]);
        push_describe(&mut script, b'P', "");
        push_execute(&mut script, "");
        push_sync(&mut script);
        push_terminate(&mut script);
        let msgs = run(script);
        assert!(msgs.iter().any(|(t, _)| *t == b'2'), "BindComplete");
        assert!(msgs
            .iter()
            .any(|(t, p)| *t == b'C' && p.starts_with(b"SELECT 0")));
    }
}
