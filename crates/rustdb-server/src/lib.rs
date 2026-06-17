//! Server-side library for rustdb.
//!
//! Currently exposes the PostgreSQL wire-protocol front end ([`pgwire`]), so
//! standard PostgreSQL clients and drivers can talk to the engine. The HTTP/JSON
//! API used by the studio UI lives in the `rustdb-server` binary.

#![forbid(unsafe_code)]

pub mod pgwire;
