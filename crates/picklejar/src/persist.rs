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

use picklejar_sql::statement::DataType;
use picklejar_sql::Value;

/// Atomically write `body` to `path` with a CRC32 integrity header, so a later
/// flipped byte (radiation, a failing disk) is detected on load rather than
/// silently applied. The on-disk format is one header line, `<crc32-hex>`, over
/// the body bytes, followed by the body itself. The write goes to a sibling temp
/// file and is renamed into place, so a crash mid-write never leaves a torn file.
///
/// # Errors
///
/// Returns an I/O error if the temp file cannot be written or renamed.
fn write_checked(path: &Path, body: &str) -> io::Result<()> {
    let crc = picklejar_storage::crc32::crc32(body.as_bytes());
    let mut out = String::with_capacity(body.len() + 16);
    let _ = writeln!(out, "{crc:08x}");
    out.push_str(body);
    let tmp = path.with_extension("pjtmp");
    fs::write(&tmp, out.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Read a file written by [`write_checked`], verifying its CRC32 header. Returns
/// `Ok(None)` if the file does not exist (a brand-new database), `Ok(Some(body))`
/// if the header matches, and an error if the file is present but its checksum
/// does not match its body, so a corrupted catalog, policy, or grant file is
/// refused rather than trusted.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read, has no valid
/// header, or fails its checksum.
fn read_checked(path: &Path) -> io::Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some((header, body)) = text.split_once('\n') else {
        return Err(corrupt_sidecar(path, "missing integrity header"));
    };
    let Ok(expected) = u32::from_str_radix(header.trim(), 16) else {
        return Err(corrupt_sidecar(path, "unreadable integrity header"));
    };
    if picklejar_storage::crc32::crc32(body.as_bytes()) != expected {
        return Err(corrupt_sidecar(path, "checksum mismatch"));
    }
    Ok(Some(body.to_string()))
}

/// An `InvalidData` error naming a corrupted sidecar file and why.
fn corrupt_sidecar(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupted metadata file {}: {why}", path.display()),
    )
}

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

fn type_tag(t: DataType) -> String {
    match t {
        DataType::Int => "INT".to_string(),
        DataType::Float => "FLOAT".to_string(),
        DataType::Bool => "BOOL".to_string(),
        DataType::Text => "TEXT".to_string(),
        DataType::Date => "DATE".to_string(),
        DataType::Timestamp => "TIMESTAMP".to_string(),
        DataType::Json => "JSON".to_string(),
        DataType::Decimal => "DECIMAL".to_string(),
        // The dimension rides along in the token (whitespace-free) so a reopened
        // column rebuilds its declared width.
        DataType::Vector(n) => format!("VECTOR({n})"),
    }
}

fn parse_type(s: &str) -> Option<DataType> {
    match s {
        "INT" => Some(DataType::Int),
        "FLOAT" => Some(DataType::Float),
        "BOOL" => Some(DataType::Bool),
        "TEXT" => Some(DataType::Text),
        "DATE" => Some(DataType::Date),
        "TIMESTAMP" => Some(DataType::Timestamp),
        "JSON" => Some(DataType::Json),
        "DECIMAL" => Some(DataType::Decimal),
        _ => {
            let inner = s.strip_prefix("VECTOR(")?.strip_suffix(')')?;
            Some(DataType::Vector(inner.parse().ok()?))
        }
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
        Some(Value::Date(n)) => format!("d{n}"),
        Some(Value::Timestamp(n)) => format!("t{n}"),
        Some(Value::Decimal(m, scale)) => format!("D{m}v{scale}"),
        // The component list is already whitespace-free (e.g. `[1,2,3]`).
        Some(Value::Vector(v)) => format!("V{}", picklejar_sql::ast::format_vector(v)),
        Some(Value::Text(s) | Value::Json(s)) => {
            // Text and JSON share the hex form; the leading tag records which.
            let tag = if matches!(default, Some(Value::Json(_))) {
                'j'
            } else {
                's'
            };
            let mut out = String::from(tag);
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
        'd' => Value::Date(rest.parse().map_err(|_| invalid())?),
        't' => Value::Timestamp(rest.parse().map_err(|_| invalid())?),
        'D' => {
            let (m, scale) = rest.split_once('v').ok_or_else(invalid)?;
            Value::Decimal(
                m.parse().map_err(|_| invalid())?,
                scale.parse().map_err(|_| invalid())?,
            )
        }
        'V' => Value::Vector(picklejar_sql::ast::parse_vector(rest).ok_or_else(invalid)?),
        's' | 'j' => {
            if rest.len() % 2 != 0 {
                return Err(invalid());
            }
            let bytes = (0..rest.len())
                .step_by(2)
                .map(|j| u8::from_str_radix(&rest[j..j + 2], 16).map_err(|_| invalid()))
                .collect::<io::Result<Vec<u8>>>()?;
            let s = String::from_utf8(bytes).map_err(|_| invalid())?;
            if tag == 'j' {
                Value::Json(s)
            } else {
                Value::Text(s)
            }
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
    write_checked(path, &serialize(records))
}

/// Serialize `records` to the catalog body string.
///
/// These are the same bytes [`save`] writes under its integrity header, exposed
/// so the engine can log an identical catalog snapshot to the WAL.
#[must_use]
pub fn serialize(records: &[TableRecord]) -> String {
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
    out
}

/// Write an already-serialized catalog `body` to `path` under the integrity
/// header.
///
/// Uses the same framing [`save`] does, so the engine can reconstruct the
/// sidecar from a WAL catalog snapshot on open, making the log authoritative
/// for the schema.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_serialized(path: &Path, body: &str) -> io::Result<()> {
    write_checked(path, body)
}

/// Read records from `path`. An absent file yields an empty list (a brand-new
/// database).
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read or parsed.
pub fn load(path: &Path) -> io::Result<Vec<TableRecord>> {
    let Some(text) = read_checked(path)? else {
        return Ok(Vec::new());
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
    write_checked(path, &out)?;
    Ok(())
}

/// Read the transaction watermark and aborted xids. An absent file yields the
/// fresh-database default `(1, [])`.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read or parsed.
pub fn load_txn(path: &Path) -> io::Result<(u64, Vec<u64>)> {
    let Some(text) = read_checked(path)? else {
        return Ok((1, Vec::new()));
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
    write_checked(path, &out)?;
    Ok(())
}

/// Read the views as `(name, sql)` pairs. An absent file yields an empty list.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read.
pub fn load_views(path: &Path) -> io::Result<Vec<(String, String)>> {
    let Some(text) = read_checked(path)? else {
        return Ok(Vec::new());
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
        /// `ON DELETE` action as a compact token (e.g. `cascade`).
        on_delete: String,
        /// `ON UPDATE` action as a compact token.
        on_update: String,
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
                on_delete,
                on_update,
            } => {
                let _ = writeln!(
                    out,
                    "{table} F {column} {parent_table} {parent_column} {on_delete} {on_update}"
                );
            }
        }
    }
    write_checked(path, &out)?;
    Ok(())
}

/// Read constraints. An absent file yields an empty list.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read or parsed.
pub fn load_constraints(path: &Path) -> io::Result<Vec<Constraint>> {
    let Some(text) = read_checked(path)? else {
        return Ok(Vec::new());
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
                // 3 fields is the legacy form (no referential actions); 5 adds
                // the ON DELETE / ON UPDATE action tokens.
                let (column, parent_table, parent_column, on_delete, on_update) = match parts[..] {
                    [c, pt, pc] => (c, pt, pc, "noaction", "noaction"),
                    [c, pt, pc, od, ou] => (c, pt, pc, od, ou),
                    _ => return Err(invalid()),
                };
                out.push(Constraint::ForeignKey {
                    table: table.to_string(),
                    column: column.to_string(),
                    parent_table: parent_table.to_string(),
                    parent_column: parent_column.to_string(),
                    on_delete: on_delete.to_string(),
                    on_update: on_update.to_string(),
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
    write_checked(path, &out)?;
    Ok(())
}

/// Read the serial columns as `(table, column)` pairs. An absent file yields an
/// empty list.
///
/// # Errors
///
/// Returns an I/O error if the file exists but cannot be read.
pub fn load_sequences(path: &Path) -> io::Result<Vec<(String, String)>> {
    let Some(text) = read_checked(path)? else {
        return Ok(Vec::new());
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

/// The persisted access-control state: roles, table grants, role memberships,
/// and table ownership. Used to serialize the security catalog to its sidecar.
#[derive(Debug, Default, Clone)]
pub struct AclData {
    /// `(name, [superuser, login, createrole, bypassrls, has_password])`.
    pub roles: Vec<(String, [bool; 5])>,
    /// `(grantee, table, privilege_bits)`.
    pub grants: Vec<(String, String, u8)>,
    /// `(member, group)` membership edges.
    pub members: Vec<(String, String)>,
    /// `(table, owner)` pairs.
    pub owners: Vec<(String, String)>,
}

/// Persist the access-control state, one tagged record per line, atomically.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_acl(path: &Path, acl: &AclData) -> io::Result<()> {
    let mut out = String::new();
    for (name, [su, login, cr, brls, pw]) in &acl.roles {
        let _ = writeln!(
            out,
            "role {name} {} {} {} {} {}",
            u8::from(*su),
            u8::from(*login),
            u8::from(*cr),
            u8::from(*brls),
            u8::from(*pw),
        );
    }
    for (grantee, table, bits) in &acl.grants {
        let _ = writeln!(out, "grant {grantee} {table} {bits}");
    }
    for (member, group) in &acl.members {
        let _ = writeln!(out, "member {member} {group}");
    }
    for (table, owner) in &acl.owners {
        let _ = writeln!(out, "owner {table} {owner}");
    }
    write_checked(path, &out)?;
    Ok(())
}

/// Read the access-control state. An absent file yields an empty set.
///
/// # Errors
///
/// Returns an I/O error if the file exists but is unreadable or malformed.
pub fn load_acl(path: &Path) -> io::Result<AclData> {
    let Some(text) = read_checked(path)? else {
        return Ok(AclData::default());
    };
    let mut acl = AclData::default();
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        match toks.first().copied() {
            Some("role") if toks.len() == 7 => {
                let flag = |i: usize| toks[i] == "1";
                acl.roles.push((
                    toks[1].to_string(),
                    [flag(2), flag(3), flag(4), flag(5), flag(6)],
                ));
            }
            Some("grant") if toks.len() == 4 => {
                acl.grants.push((
                    toks[1].to_string(),
                    toks[2].to_string(),
                    toks[3].parse().map_err(|_| invalid())?,
                ));
            }
            Some("member") if toks.len() == 3 => {
                acl.members.push((toks[1].to_string(), toks[2].to_string()));
            }
            Some("owner") if toks.len() == 3 => {
                acl.owners.push((toks[1].to_string(), toks[2].to_string()));
            }
            None => {}
            _ => return Err(invalid()),
        }
    }
    Ok(acl)
}

/// The persisted row-level-security state: per-table flags and the policy
/// statements (stored as their canonical SQL text, re-parsed on load).
#[derive(Debug, Default, Clone)]
pub struct RlsData {
    /// `(table, enabled, forced)`.
    pub flags: Vec<(String, bool, bool)>,
    /// Each a full `CREATE POLICY ...` statement.
    pub policies: Vec<String>,
}

/// Persist the row-level-security state, one record per line, atomically.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_rls(path: &Path, rls: &RlsData) -> io::Result<()> {
    write_checked(path, &serialize_rls(rls))
}

/// Serialize the row-level-security state to its body string.
///
/// These are the same bytes [`save_rls`] writes under its integrity header,
/// exposed so the engine can log an identical isolation snapshot to the WAL.
#[must_use]
pub fn serialize_rls(rls: &RlsData) -> String {
    let mut out = String::new();
    for (table, enabled, forced) in &rls.flags {
        let _ = writeln!(
            out,
            "flags {table} {} {}",
            u8::from(*enabled),
            u8::from(*forced)
        );
    }
    for policy in &rls.policies {
        let _ = writeln!(out, "policy {policy}");
    }
    out
}

/// Write an already-serialized row-level-security `body` to `path` under the
/// integrity header.
///
/// Lets the engine reconstruct the `.pol` sidecar from a WAL snapshot on open,
/// making the log authoritative for tenant isolation.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_rls_serialized(path: &Path, body: &str) -> io::Result<()> {
    write_checked(path, body)
}

/// Read the row-level-security state. An absent file yields an empty set.
///
/// # Errors
///
/// Returns an I/O error if the file exists but is unreadable or malformed.
pub fn load_rls(path: &Path) -> io::Result<RlsData> {
    let Some(text) = read_checked(path)? else {
        return Ok(RlsData::default());
    };
    let mut rls = RlsData::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("flags ") {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.len() != 3 {
                return Err(invalid());
            }
            rls.flags
                .push((toks[0].to_string(), toks[1] == "1", toks[2] == "1"));
        } else if let Some(sql) = line.strip_prefix("policy ") {
            rls.policies.push(sql.to_string());
        } else if !line.trim().is_empty() {
            return Err(invalid());
        }
    }
    Ok(rls)
}

/// One persisted variable-key (`CREATE INDEX`) secondary index.
#[derive(Debug, Clone)]
pub struct MultiIndexRecord {
    /// The table it indexes.
    pub table: String,
    /// The index name.
    pub name: String,
    /// Root page of its variable-key B+ tree.
    pub root: u64,
    /// Distinct values in the leading column at build time (for the cost model).
    pub distinct: u64,
    /// Whether the index enforces uniqueness of the indexed value tuple.
    pub unique: bool,
    /// The indexed column names, in index order.
    pub columns: Vec<String>,
}

/// Persist the variable-key secondary indexes, one per line, atomically:
/// `table name root distinct unique col1 col2 ...`.
///
/// # Errors
///
/// Returns an I/O error if the file cannot be written or renamed.
pub fn save_multi_indexes(path: &Path, records: &[MultiIndexRecord]) -> io::Result<()> {
    let mut out = String::new();
    for r in records {
        let _ = write!(
            out,
            "{} {} {} {} {}",
            r.table,
            r.name,
            r.root,
            r.distinct,
            u8::from(r.unique),
        );
        for col in &r.columns {
            let _ = write!(out, " {col}");
        }
        out.push('\n');
    }
    write_checked(path, &out)?;
    Ok(())
}

/// Read the variable-key secondary indexes. An absent file yields an empty list.
///
/// # Errors
///
/// Returns an I/O error if the file exists but is unreadable or malformed.
pub fn load_multi_indexes(path: &Path) -> io::Result<Vec<MultiIndexRecord>> {
    let Some(text) = read_checked(path)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        if toks.len() < 6 {
            return Err(invalid());
        }
        out.push(MultiIndexRecord {
            table: toks[0].to_string(),
            name: toks[1].to_string(),
            root: parse_u64(toks[2])?,
            distinct: parse_u64(toks[3])?,
            unique: toks[4] == "1",
            columns: toks[5..].iter().map(|s| (*s).to_string()).collect(),
        });
    }
    Ok(out)
}

/// Persist the fault log as `seq page kind` lines, integrity-checked like the
/// other sidecars. `kind` is a single whitespace-free token.
///
/// # Errors
///
/// Returns an I/O error if the sidecar cannot be written.
pub fn save_fault_log(path: &Path, entries: &[(u64, u64, String)]) -> io::Result<()> {
    let mut out = String::new();
    for (seq, page, kind) in entries {
        let _ = writeln!(out, "{seq} {page} {kind}");
    }
    write_checked(path, &out)
}

/// Read the fault log written by [`save_fault_log`]. An absent file is an empty
/// log; a present file with a bad checksum is an error.
///
/// # Errors
///
/// Returns an I/O error if the file is present but cannot be read or fails its
/// checksum.
pub fn load_fault_log(path: &Path) -> io::Result<Vec<(u64, u64, String)>> {
    let Some(text) = read_checked(path)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let mut toks = line.split_whitespace();
        let (Some(seq), Some(page), Some(kind)) = (toks.next(), toks.next(), toks.next()) else {
            continue;
        };
        let (Ok(seq), Ok(page)) = (parse_u64(seq), parse_u64(page)) else {
            continue;
        };
        out.push((seq, page, kind.to_string()));
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
