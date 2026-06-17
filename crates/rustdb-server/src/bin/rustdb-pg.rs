//! PostgreSQL-wire-protocol server for rustdb.
//!
//! Opens one [`Database`] and serves it to PostgreSQL clients (`psql`, drivers,
//! GUI tools) over the real frontend/backend protocol. The engine runs on its
//! own thread behind an [`EngineActor`]; each accepted connection gets its own
//! thread and a session handle, so many clients are served concurrently.
//!
//! ```text
//! cargo run --bin rustdb-pg -- --database mydb.db --port 5433
//! psql -h 127.0.0.1 -p 5433 -U postgres
//! ```

use std::net::TcpListener;
use std::thread;

use clap::Parser as ClapParser;
use rustdb::Database;
use rustdb_server::engine::EngineActor;
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
    // The engine runs on its own thread and opens the database there (it is
    // `!Send`); `spawn` reports any open error back to us.
    let db_path = args.database.clone();
    let actor =
        match EngineActor::spawn(move || Database::open(&db_path).map_err(|e| e.to_string())) {
            Ok(actor) => actor,
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

    // Hand each accepted connection its own thread and session handle.
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let mut session = actor.session();
                thread::spawn(move || {
                    if let Err(e) = pgwire::serve(&mut session, &mut stream) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
}
