//! Server-side library for picklejar.
//!
//! Currently exposes the PostgreSQL wire-protocol front end ([`pgwire`]), so
//! standard PostgreSQL clients and drivers can talk to the engine. The HTTP/JSON
//! API used by the studio UI lives in the `picklejar-server` binary.

#![forbid(unsafe_code)]

pub mod engine;
pub mod pgwire;
pub mod scram;
pub mod sha256;
