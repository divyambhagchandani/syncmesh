//! Error and exit-reason types for the player crate.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MpvError {
    #[error("failed to spawn mpv: {0}")]
    Spawn(#[source] io::Error),
    #[error("failed to connect to mpv IPC endpoint after {waited_ms} ms: {source}")]
    Connect {
        waited_ms: u64,
        #[source]
        source: io::Error,
    },
    #[error("mpv IPC I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("mpv sent a malformed JSON frame: {0}")]
    BadJson(#[source] serde_json::Error),
    #[error("mpv command failed: {0}")]
    CommandFailed(String),
    #[error("mpv handle is closed")]
    Closed,
}

/// How the mpv process ended. Surfaced on the event stream as
/// [`crate::MpvEvent::Shutdown`]; a richer version is returned from the
/// wait/exit handle for the bin crate to log.
#[derive(Debug)]
pub enum MpvExit {
    /// mpv exited cleanly (quit command, user closed window, end of file with
    /// `--keep-open=no`).
    Clean { code: Option<i32> },
    /// mpv crashed or was killed.
    Crashed { code: Option<i32> },
    /// The handle was dropped — we killed mpv on shutdown.
    Killed,
    /// We never managed to talk to mpv in the first place.
    SpawnFailed(MpvError),
}
