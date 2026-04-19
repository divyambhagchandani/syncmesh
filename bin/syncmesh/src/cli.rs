//! CLI surface for syncmesh. Two subcommands today: `create` and `join`.
//!
//! `--no-mpv` lets the binary run without spawning mpv — useful for smoke-
//! testing the mesh layer on a headless box. When mpv is disabled, inbound
//! `MpvCommand`s are logged but not dispatched.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "syncmesh", version, about = "P2P Syncplay alternative for mpv")]
pub struct Cli {
    /// Subcommand to run. When absent we print a short splash and exit 0 so
    /// `syncmesh` by itself doesn't error out at users who forgot the verb.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Nickname shown to other peers.
    #[arg(long, global = true)]
    pub nickname: Option<String>,

    /// Run without spawning mpv. Useful for CI / smoke tests.
    #[arg(long, global = true)]
    pub no_mpv: bool,

    /// Skip the terminal UI. Runs the event loop headless (useful for logs-
    /// only runs or when stdin isn't a TTY).
    #[arg(long, global = true)]
    pub no_ui: bool,

    /// Path to the mpv binary. Defaults to `mpv` on PATH.
    #[arg(long, global = true)]
    pub mpv_binary: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new room and print a ticket for friends to join.
    Create {
        /// File to open in mpv at startup.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Join an existing room using a ticket.
    Join {
        /// Ticket string (begins with `syncmesh1`).
        ticket: String,
        /// File to open in mpv at startup. Should match the host's file.
        #[arg(long)]
        file: Option<PathBuf>,
    },
}
