//! PostgreSQL-wire-protocol server for rustdb.
//!
//! Opens one [`Database`] and serves it to PostgreSQL clients (`psql`, drivers,
//! GUI tools) over the real frontend/backend protocol. Connections are served
//! one at a time because the engine is single-threaded.
//!
//! ```text
//! cargo run --bin rustdb-pg -- --database mydb.db --port 5433
//! psql -h 127.0.0.1 -p 5433 -U postgres
//! ```

use std::net::TcpListener;

use clap::Parser as ClapParser;
use rustdb::Database;
use rustdb_server::pgwire;

#[derive(Debug, ClapParser)]
#[command(
    name = "rustdb-pg",
    version,
    about = "PostgreSQL wire-protocol server for rustdb"
)]
struct Args {
    /// Path to the database file.
    #[arg(short, long, default_value = "rustdb.db")]
    database: String,
    /// TCP port to listen on.
    #[arg(short, long, default_value_t = 5433)]
    port: u16,
}

fn main() {
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
        "rustdb-pg listening on 127.0.0.1:{port} (database {db_path})\n\
         connect with: psql -h 127.0.0.1 -p {port} -U postgres",
        port = args.port,
        db_path = args.database,
    );

    // The engine is single-threaded, so connections are served serially.
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(e) = pgwire::serve(&mut db, &mut stream) {
                    eprintln!("connection error: {e}");
                }
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
}
