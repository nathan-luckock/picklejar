//! psql-style CLI for rustdb.

use clap::Parser;

#[derive(Debug, Parser)]
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
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    println!("rustdb 0.0.1 - type \\q to quit");
    println!("(connected to {})", args.database);
    // TODO: REPL loop wired up once parser + executor are real.
}
