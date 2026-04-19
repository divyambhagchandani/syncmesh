//! Library surface of the syncmesh binary crate.
//!
//! This exists so integration tests under `tests/` can import the same types
//! the binary uses. Production code paths still live in `main.rs`; this
//! module tree is only re-exported so tests can construct an `App` without
//! duplicating code.

pub mod addrs;
pub mod app;
pub mod cli;
pub mod config;
pub mod echo;
pub mod media;
pub mod peer_task;
pub mod ui;
