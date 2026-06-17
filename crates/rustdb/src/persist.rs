//! Catalog persistence: a small sidecar file that lets a reopened database
//! rediscover its tables.
//!
//! The engine writes one line per table to `<base>.meta` after each statement
//! that changes the schema or a table's anchor pages. A line records the
//! table name, its index B+ tree root page, its current version heap page,
//! the next rowid, then the columns and indexes. On open the engine reads
//! this back to rebuild the in-memory catalog and the per-table descriptors,
//! so the existing on-disk pages are reachable again.
//!
//! The format is line-oriented and space-delimited. SQL identifiers contain no
//! spaces, so no escaping is needed.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use rustdb_sql::statement::DataType;
use rustdb_sql::Value;

/// One table's persisted metadata.
#[derive(Debug, Clone)]
pub struct TableRecord {
    /// Table name.
    pub name: String,
    /// Columns as `(name, type, primary_key, not_null, unique, default)`.
    pub columns: Vec<(String, DataType, bool, bool, bool, Option<Value>)>,
    /// Indexes as `(index_name, column)`.
    pub indexes: Vec<(String, String)>,
    /// Physical secondary indexes as `(column, root_page)`: the B+ tree root
    /// for each indexed column. A subset of `indexes` (only columns with a
    /// real index structure: unique INT columns).
    pub secondary: Vec<(String, u64)>,
    /// Index B+ tree root page.
    pub index_root: u64,
    /// Current version heap page.
    pub version_page: u64,
    /// Next auto-increment rowid.
    pub next_rowid: u64,
}

const fn type_tag(t: DataType) -> &'static str {
    match t {
        DataType::Int => "INT",
        DataType::Float => "FLOAT",
        DataType::Bool => "BOOL",
        DataType::Text => "TEXT",
    }
}

fn parse_type(s: &str) -> Option<DataType> {
    match s {
        "INT" => Some(DataType::Int),
        "FLOAT" => Some(DataType::Float),
        "BOOL" => Some(DataType::Bool),
        "TEXT" => Some(DataType::Text),
        _ => None,
    }
}

/// Encode a column DEFAULT as a single whitespace-free token. Text is
/// hex-encoded so values with spaces survive the space-delimited format.
fn encode_default(default: Option<&Value>) -> String {
    match default {
        None => "-".to_string(),
        Some(Value::Null) => "N".to_string(),
        Some(Value::Int(n)) => format!("i{n}"),
        Some(Value::Float(x)) => format!("f{}", x.to_bits()),
        Some(Value::Bool(b)) => format!("B{}", u8::from(*b)),
        Some(Value::Text(s)) => {
            let mut out = String::from("s");
            for b in s.bytes() {
                let _ = write!(out, "{b:02x}");
            }
            out
        }
    }
}

/// Inverse of [`encode_default`].
fn decode_default(tok: &str) -> io::Result<Option<Value>> {
    let mut chars = tok.chars();
    let tag = chars.next().ok_or_else(invalid)?;
    let rest = &tok[tag.len_utf8()..];
    let v = match tag {
        '-' => return Ok(None),
        'N' => Value::Null,
        'i' => Value::Int(rest.parse().map_err(|_| invalid())?),
        'f' => Value::Float(f64::from_bits(rest.parse().map_err(|_| invalid())?)),
        'B' => Value::Bool(rest == "1"),
        's' => {
            if rest.len() % 2 != 0 {
                return Err(invalid());
            }
            let bytes = (0..rest.len())
                .step_by(2)
                .map(|j| u8::from_str_radix(&rest[j..j + 2], 16).map_err(|_| invalid()))
                .collect::<io::Result<Vec<u8>>>()?;
            Value::Text(String::from_utf8(bytes).map_err(|_| invalid())?)
        }
        _ => return Err(invalid()),
    };
    Ok(Some(v))
}

/// Write `records` to `path` atomically: write a temp file, then rename it
/// over the target so a crash never leaves a half-written catalog.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save(path: &Path, records: &[TableRecord]) -> io::Result<()> {
    let mut out = String::new();
    for r in records {
        let _ = write!(
            out,
            "{} {} {} {} {}",
            r.name,
            r.index_root,
            r.version_page,
            r.next_rowid,
            r.columns.len()
        );
        for (name, ty, pk, not_null, unique, default) in &r.columns {
            let _ = write!(
                out,
                " {name} {} {} {} {} {}",
                type_tag(*ty),
                u8::from(*pk),
                u8::from(*not_null),
                u8::from(*unique),
                encode_default(default.as_ref()),
            );
        }
        let _ = write!(out, " {}", r.indexes.len());
        for (name, col) in &r.indexes {
            let _ = write!(out, " {name} {col}");
        }
        let _ = write!(out, " {}", r.secondary.len());
        for (col, root) in &r.secondary {
            let _ = write!(out, " {col} {root}");
        }
        out.push('\n');
    }
    let tmp = path.with_extension("meta.tmp");
    fs::write(&tmp, out.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Read records from `path`. An absent file yields an empty list (a brand-new
/// database).
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read or parsed.
pub fn load(path: &Path) -> io::Result<Vec<TableRecord>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut records = Vec::new();
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        let mut i = 0;
        let name = field(&toks, &mut i)?.to_string();
        let index_root = parse_u64(field(&toks, &mut i)?)?;
        let version_page = parse_u64(field(&toks, &mut i)?)?;
        let next_rowid = parse_u64(field(&toks, &mut i)?)?;

        let ncols = parse_usize(field(&toks, &mut i)?)?;
        let mut columns = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let cname = field(&toks, &mut i)?.to_string();
            let ty = parse_type(field(&toks, &mut i)?).ok_or_else(invalid)?;
            let pk = field(&toks, &mut i)? == "1";
            let not_null = field(&toks, &mut i)? == "1";
            let unique = field(&toks, &mut i)? == "1";
            let default = decode_default(field(&toks, &mut i)?)?;
            columns.push((cname, ty, pk, not_null, unique, default));
        }

        let nidx = parse_usize(field(&toks, &mut i)?)?;
        let mut indexes = Vec::with_capacity(nidx);
        for _ in 0..nidx {
            let iname = field(&toks, &mut i)?.to_string();
            let icol = field(&toks, &mut i)?.to_string();
            indexes.push((iname, icol));
        }

        // The secondary-index section is optional, so a catalog written before
        // physical indexes existed still loads (as having none).
        let secondary = if i < toks.len() {
            let nsec = parse_usize(field(&toks, &mut i)?)?;
            let mut v = Vec::with_capacity(nsec);
            for _ in 0..nsec {
                let col = field(&toks, &mut i)?.to_string();
                let root = parse_u64(field(&toks, &mut i)?)?;
                v.push((col, root));
            }
            v
        } else {
            Vec::new()
        };

        records.push(TableRecord {
            name,
            columns,
            indexes,
            secondary,
            index_root,
            version_page,
            next_rowid,
        });
    }
    Ok(records)
}

/// Persist the transaction watermark and the aborted-xid set to `path`,
/// atomically (temp file then rename). The single line is `next_xid` followed
/// by the aborted xids, all space-separated.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_txn(path: &Path, next_xid: u64, aborted: &[u64]) -> io::Result<()> {
    let mut out = String::new();
    let _ = write!(out, "{next_xid}");
    for x in aborted {
        let _ = write!(out, " {x}");
    }
    out.push('\n');
    let tmp = path.with_extension("txn.tmp");
    fs::write(&tmp, out.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the transaction watermark and aborted xids. An absent file yields the
/// fresh-database default `(1, [])`.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read or parsed.
pub fn load_txn(path: &Path) -> io::Result<(u64, Vec<u64>)> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((1, Vec::new())),
        Err(e) => return Err(e),
    };
    let mut nums = text.split_whitespace().map(parse_u64);
    let next_xid = match nums.next() {
        Some(n) => n?,
        None => return Ok((1, Vec::new())),
    };
    let aborted = nums.collect::<io::Result<Vec<u64>>>()?;
    Ok((next_xid, aborted))
}

/// Persist the views as `(name, sql)` pairs, one per line.
///
/// Each line is the view name (a whitespace-free identifier), a space, then
/// the view's defining query as canonical single-line SQL. Written atomically
/// (temp file then rename).
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_views(path: &Path, views: &[(String, String)]) -> io::Result<()> {
    let mut out = String::new();
    for (name, sql) in views {
        let _ = writeln!(out, "{name} {sql}");
    }
    let tmp = path.with_extension("view.tmp");
    fs::write(&tmp, out.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the views as `(name, sql)` pairs. An absent file yields an empty list.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read.
pub fn load_views(path: &Path) -> io::Result<Vec<(String, String)>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut views = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (name, sql) = line.split_once(' ').ok_or_else(invalid)?;
        views.push((name.to_string(), sql.to_string()));
    }
    Ok(views)
}

/// A persisted table constraint.
#[derive(Debug, Clone)]
pub enum Constraint {
    /// `CHECK (predicate)` on `table`; `sql` is the predicate's canonical SQL.
    Check {
        /// Table the check belongs to.
        table: String,
        /// The predicate as single-line SQL.
        sql: String,
    },
    /// A single-column foreign key on `table`.
    ForeignKey {
        /// Child table.
        table: String,
        /// Referencing column in the child table.
        column: String,
        /// Referenced (parent) table.
        parent_table: String,
        /// Referenced column in the parent table.
        parent_column: String,
    },
}

/// Persist constraints, one per line.
///
/// A check is `<table> C <check sql>`; a foreign key is
/// `<table> F <column> <parent_table> <parent_column>`. Identifiers are
/// whitespace-free, and a check's SQL is the rest of its line.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_constraints(path: &Path, constraints: &[Constraint]) -> io::Result<()> {
    let mut out = String::new();
    for c in constraints {
        match c {
            Constraint::Check { table, sql } => {
                let _ = writeln!(out, "{table} C {sql}");
            }
            Constraint::ForeignKey {
                table,
                column,
                parent_table,
                parent_column,
            } => {
                let _ = writeln!(out, "{table} F {column} {parent_table} {parent_column}");
            }
        }
    }
    let tmp = path.with_extension("cons.tmp");
    fs::write(&tmp, out.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Read constraints. An absent file yields an empty list.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read or parsed.
pub fn load_constraints(path: &Path) -> io::Result<Vec<Constraint>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (table, rest) = line.split_once(' ').ok_or_else(invalid)?;
        let (kind, body) = rest.split_once(' ').ok_or_else(invalid)?;
        match kind {
            "C" => out.push(Constraint::Check {
                table: table.to_string(),
                sql: body.to_string(),
            }),
            "F" => {
                let parts: Vec<&str> = body.split_whitespace().collect();
                let [column, parent_table, parent_column] = parts[..] else {
                    return Err(invalid());
                };
                out.push(Constraint::ForeignKey {
                    table: table.to_string(),
                    column: column.to_string(),
                    parent_table: parent_table.to_string(),
                    parent_column: parent_column.to_string(),
                });
            }
            _ => return Err(invalid()),
        }
    }
    Ok(out)
}

/// Persist the serial (auto-increment) columns as `(table, column)` pairs, one
/// per line, atomically.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_sequences(path: &Path, columns: &[(String, String)]) -> io::Result<()> {
    let mut out = String::new();
    for (table, column) in columns {
        let _ = writeln!(out, "{table} {column}");
    }
    let tmp = path.with_extension("seq.tmp");
    fs::write(&tmp, out.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the serial columns as `(table, column)` pairs. An absent file yields an
/// empty list.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read.
pub fn load_sequences(path: &Path) -> io::Result<Vec<(String, String)>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (table, column) = line.split_once(' ').ok_or_else(invalid)?;
        out.push((table.to_string(), column.to_string()));
    }
    Ok(out)
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed catalog metadata")
}

fn field<'a>(toks: &[&'a str], i: &mut usize) -> io::Result<&'a str> {
    let v = toks.get(*i).copied().ok_or_else(invalid)?;
    *i += 1;
    Ok(v)
}

fn parse_u64(s: &str) -> io::Result<u64> {
    s.parse().map_err(|_| invalid())
}

fn parse_usize(s: &str) -> io::Result<usize> {
    s.parse().map_err(|_| invalid())
}
