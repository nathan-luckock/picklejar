//! PostgreSQL-wire-protocol server for picklejar.
//!
//! Opens one [`Database`] and serves it to PostgreSQL clients (`psql`, drivers,
//! GUI tools) over the real frontend/backend protocol. The engine runs on its
//! own thread behind an [`EngineActor`]; each accepted connection gets its own
//! thread and a session handle, so many clients are served concurrently.
//!
//! ```text
//! cargo run --bin picklejar-pg -- --database mydb.db --port 5433
//! psql -h 127.0.0.1 -p 5433 -U postgres
//! ```

use std::net::TcpListener;
use std::thread;

use std::sync::Arc;

use clap::Parser as ClapParser;
use picklejar::Database;
use picklejar_server::engine::EngineActor;
use picklejar_server::pgwire;
use picklejar_server::scram::{Auth, Credentials};

#[derive(Debug, ClapParser)]
#[command(
    name = "picklejar-pg",
    version,
    about = "PostgreSQL wire-protocol server for picklejar"
)]
struct Args {
    /// Path to the database file.
    #[arg(short, long, default_value = "picklejar.db")]
    database: String,
    /// TCP port to listen on.
    #[arg(short, long, default_value_t = 5433)]
    port: u16,
    /// Account name a client must connect as when a password is set.
    #[arg(short, long, default_value = "postgres")]
    user: String,
    /// Require SCRAM-SHA-256 authentication with this password. Omitted means
    /// trust authentication (any user, no password).
    #[arg(long)]
    password: Option<String>,
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
    // Derive the SCRAM verifier once (shared across connections) when a password
    // is configured; otherwise accept any user with trust authentication.
    let auth = Arc::new(match &args.password {
        Some(pw) => Auth::Scram(Credentials::new(&args.user, pw)),
        None => Auth::Trust,
    });
    let auth_note = if args.password.is_some() {
        format!("SCRAM-SHA-256 as user {}", args.user)
    } else {
        "trust (no password)".to_string()
    };
    println!(
        "picklejar-pg listening on 127.0.0.1:{port} (database {db_path}, auth: {auth_note})\n\
         connect with: psql -h 127.0.0.1 -p {port} -U {user}",
        port = args.port,
        db_path = args.database,
        user = args.user,
    );

    // Hand each accepted connection its own thread and session handle.
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let mut session = actor.session();
                let auth = Arc::clone(&auth);
                thread::spawn(move || {
                    if let Err(e) = pgwire::serve_with_auth(&mut session, &mut stream, &auth) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
}
