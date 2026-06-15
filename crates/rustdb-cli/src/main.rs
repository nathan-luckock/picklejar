//! psql-style CLI for rustdb.
//!
//! A line-oriented REPL: type SQL terminated by `;` and see the result as a
//! table, or use a backslash meta-command (`\dt`, `\d <table>`, `\q`).
//! `EXPLAIN <select>` prints the cost-annotated plan.

use std::io::{self, BufRead, Write};

use clap::Parser as ClapParser;
use rustdb::{DataType, Database, QueryOutcome, Value};

#[derive(Debug, ClapParser)]
#[command(
    name = "rustdb",
    version,
    about = "Interactive shell for the rustdb engine"
)]
struct Args {
    /// Path to the database file.
    #[arg(short, long, default_value = "rustdb.db")]
    database: String,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
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

    println!("rustdb 0.0.1  (type \\? for help, \\q to quit)");
    println!("connected to {}", args.database);
    repl(&mut db);
}

/// The read-eval-print loop. Accumulates input until a `;` terminates a
/// statement, so statements may span multiple lines.
fn repl(db: &mut Database) {
    let stdin = io::stdin();
    let mut pending = String::new();

    print_prompt(pending.is_empty());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };

        // Meta-commands are only recognized at the start of a statement.
        if pending.trim().is_empty() && line.trim_start().starts_with('\\') {
            if handle_meta(db, line.trim()) == Flow::Quit {
                return;
            }
            pending.clear();
            print_prompt(true);
            continue;
        }

        pending.push_str(&line);
        pending.push('\n');

        // Run every complete (`;`-terminated) statement now buffered.
        while let Some(idx) = pending.find(';') {
            let stmt: String = pending.drain(..=idx).collect();
            let stmt = stmt.trim().trim_end_matches(';').trim().to_string();
            if !stmt.is_empty() {
                run_and_print(db, &stmt);
            }
        }
        print_prompt(pending.trim().is_empty());
    }
    println!();
}

fn print_prompt(fresh: bool) {
    print!("{}", if fresh { "rustdb> " } else { "   ...> " });
    let _ = io::stdout().flush();
}

/// Control-flow signal from a meta-command.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// Handle a `\`-prefixed meta-command.
fn handle_meta(db: &Database, cmd: &str) -> Flow {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("\\q" | "\\quit") => return Flow::Quit,
        Some("\\dt") => {
            let names = db.table_names();
            if names.is_empty() {
                println!("no tables");
            } else {
                for n in names {
                    println!("{n}");
                }
            }
        }
        Some("\\d") => match parts.next() {
            Some(table) => describe(db, table),
            None => println!("usage: \\d <table>"),
        },
        Some("\\?" | "\\h" | "\\help") => print_help(),
        Some(other) => println!("unknown command: {other}  (\\? for help)"),
        None => {}
    }
    Flow::Continue
}

fn describe(db: &Database, table: &str) {
    match db.columns(table) {
        Some(cols) => {
            println!("Table \"{table}\"");
            for (name, ty) in cols {
                println!("  {name}  {}", type_name(ty));
            }
        }
        None => println!("no such table: {table}"),
    }
}

const fn type_name(ty: DataType) -> &'static str {
    match ty {
        DataType::Int => "INT",
        DataType::Text => "TEXT",
    }
}

fn print_help() {
    println!("commands:");
    println!("  \\dt           list tables");
    println!("  \\d <table>    describe a table");
    println!("  \\?            this help");
    println!("  \\q            quit");
    println!("any other input is run as SQL (terminate with ;)");
    println!("  EXPLAIN <select>   show the cost-based plan");
}

/// Run one SQL statement and print its result or error.
fn run_and_print(db: &mut Database, sql: &str) {
    match db.execute(sql) {
        Ok(QueryOutcome::Ddl) => println!("OK"),
        Ok(QueryOutcome::Mutation { affected }) => {
            println!("{affected} row{}", plural(affected));
        }
        Ok(QueryOutcome::Rows { columns, rows }) => print_table(&columns, &rows),
        Ok(QueryOutcome::Explain(text)) => println!("{text}"),
        Err(e) => println!("Error: {e}"),
    }
}

/// Print a result set as an aligned text table.
fn print_table(columns: &[String], rows: &[Vec<Value>]) {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(fmt_value).collect())
        .collect();

    let mut widths: Vec<usize> = columns.iter().map(String::len).collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    println!(
        "{}",
        join_padded(columns.iter().map(String::as_str), &widths)
    );
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", rule.join("-+-"));
    for row in &cells {
        println!("{}", join_padded(row.iter().map(String::as_str), &widths));
    }
    println!("({} row{})", rows.len(), plural(rows.len()));
}

/// Join cells with ` | `, left-padding each to its column width.
fn join_padded<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    cells
        .enumerate()
        .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "NULL".to_string(),
    }
}

const fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
