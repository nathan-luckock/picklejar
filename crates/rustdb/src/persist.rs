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

/// One table's persisted metadata.
#[derive(Debug, Clone)]
pub struct TableRecord {
    /// Table name.
    pub name: String,
    /// Columns as `(name, type, primary_key, not_null, unique)`.
    pub columns: Vec<(String, DataType, bool, bool, bool)>,
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
        for (name, ty, pk, not_null, unique) in &r.columns {
            let _ = write!(
                out,
                " {name} {} {} {} {}",
                type_tag(*ty),
                u8::from(*pk),
                u8::from(*not_null),
                u8::from(*unique),
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
            columns.push((cname, ty, pk, not_null, unique));
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
