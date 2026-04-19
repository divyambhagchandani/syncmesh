//! mpv IPC client wrapper for syncmesh.
//!
//! This crate owns the mpv lifecycle: it spawns the user's mpv binary with
//! `--input-ipc-server`, connects to the resulting JSON IPC endpoint (Unix
//! socket or Windows named pipe), translates [`MpvCommand`]s into mpv command
//! frames, and translates mpv property-change events into the [`MpvEvent`]
//! stream the rest of the app consumes.
//!
//! The IPC wire protocol is line-delimited JSON, documented at
//! <https://mpv.io/manual/stable/#json-ipc>. We implement it directly rather
//! than depend on a third-party client — the surface area is small, the
//! protocol is stable since 2015 (plan decision 18's risk register), and
//! owning the transport lets us mock cleanly with [`tokio::io::duplex`] in
//! tests.

pub mod command;
pub mod error;
pub mod event;
pub mod ipc;
pub mod process;
pub mod transport;

pub use command::MpvCommand;
pub use error::{MpvError, MpvExit};
pub use event::MpvEvent;
pub use ipc::{IpcClient, OBSERVED_PROPERTIES};
pub use process::{MpvHandle, SpawnOptions, spawn};
pub use transport::{IpcTransport, connect_transport, generate_ipc_path};
