//! Run a real picklejar replication node over TCP.
//!
//! Start a small cluster by hand:
//! ```text
//! pjnode --id 0 --port 7000 --peer 1@127.0.0.1:7001 --peer 2@127.0.0.1:7002
//! pjnode --id 1 --port 7001 --peer 0@127.0.0.1:7000 --peer 2@127.0.0.1:7002
//! pjnode --id 2 --port 7002 --peer 0@127.0.0.1:7000 --peer 1@127.0.0.1:7001
//! ```
//! Each node serves the put/get/pull protocol and, if given peers, runs
//! background anti-entropy to converge with them.

use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use picklejar_replication::net::{pull_into, Node};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut id: u64 = 0;
    let mut port: u16 = 7000;
    let mut peers: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--id" => {
                i += 1;
                id = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(7000);
            }
            "--peer" => {
                i += 1;
                if let Some(spec) = args.get(i) {
                    let addr = spec.split_once('@').map_or(spec.as_str(), |(_, a)| a);
                    peers.push(addr.to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }

    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not bind 0.0.0.0:{port}: {e}");
            std::process::exit(1);
        }
    };
    println!("pjnode {id} listening on 0.0.0.0:{port}; peers: {peers:?}");

    let node = Node::new(id);
    if !peers.is_empty() {
        let store = node.store();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            for peer in &peers {
                let _ = pull_into(&store, peer);
            }
        });
    }
    node.serve(&listener);
}
