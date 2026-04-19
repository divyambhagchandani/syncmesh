//! syncmesh CLI entry point.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use syncmesh_core::RoomState;
use syncmesh_net::{
    MeshConfig, MeshEndpoint, PeerLink, decode_ticket, endpoint_id_to_node, identity,
};
use syncmesh_player::{MpvHandle, SpawnOptions, spawn as mpv_spawn};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use syncmesh::app::{App, LoopEvent};
use syncmesh::cli::{Cli, Command};
use syncmesh::config::Paths;
use syncmesh::peer_task;
use syncmesh::ui::{self, UiContext, UiEvent};

const EVENT_QUEUE: usize = 1024;

#[tokio::main(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let Some(command) = cli.command else {
        print_splash();
        return Ok(());
    };

    let paths = Paths::discover()?;
    info!(config_dir = %paths.config_dir.display(), "config loaded");
    let secret =
        identity::load_or_create(&paths.identity).context("loading or generating identity")?;

    let mesh = MeshEndpoint::bind(secret, MeshConfig::default())
        .await
        .context("binding mesh endpoint")?;
    let local_node = endpoint_id_to_node(mesh.endpoint_id());
    let local_nickname = cli
        .nickname
        .clone()
        .unwrap_or_else(|| format!("peer-{local_node:?}"));

    // Launch mpv if requested.
    let (mpv_handle, mpv_events) = if cli.no_mpv {
        (None, None)
    } else {
        let file = match &command {
            Command::Create { file } | Command::Join { file, .. } => file.clone(),
        };
        let (h, rx) = spawn_mpv(cli.mpv_binary.clone(), file).await?;
        (Some(h), Some(rx))
    };

    // Room state + event channel
    let state = RoomState::new(local_node, local_nickname.clone());
    let (events_tx, events_rx) = mpsc::channel::<LoopEvent>(EVENT_QUEUE);
    let (mut app, snapshot_rx) = App::new(state, mesh.clone(), mpv_handle, events_tx.clone());

    // Ctrl-C
    {
        let tx = events_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                warn!(error = %e, "ctrl-c handler failed");
                return;
            }
            info!("received Ctrl-C");
            let _ = tx.send(LoopEvent::Shutdown).await;
        });
    }

    // mpv → app forwarder
    if let Some(mut mpv_rx) = mpv_events {
        let tx = events_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = mpv_rx.recv().await {
                if tx.send(LoopEvent::Mpv(ev)).await.is_err() {
                    break;
                }
            }
        });
    }

    // Accept loop
    peer_task::spawn_accept_task(mesh.clone(), events_tx.clone());

    // Subcommand side-effects (print/parse ticket, dial host).
    let ticket_for_ui = match &command {
        Command::Create { .. } => {
            let ticket = mesh.ticket();
            if cli.no_ui {
                println!("your ticket (share with a friend):");
                println!("{ticket}");
            } else {
                info!(%ticket, "room ticket (press `c` in UI to copy)");
            }
            Some(ticket)
        }
        Command::Join { ticket, .. } => {
            let addr = decode_ticket(ticket).context("parsing ticket")?;
            info!(peer = ?addr.id, "dialing host");
            let host_node = endpoint_id_to_node(addr.id);
            let addr_bytes = syncmesh::addrs::encode_addr(&addr);
            if let Some(bytes) = &addr_bytes {
                app.record_addr(host_node, bytes.clone());
            }
            let link = mesh.dial(addr).await.context("dialing host")?;
            let dialed: PeerLink = link;
            events_tx
                .send(LoopEvent::PeerConnected {
                    link: dialed,
                    addr_bytes,
                })
                .await
                .ok();
            None
        }
    };

    // UI task (unless disabled)
    let (ui_rx, ui_handle) = if cli.no_ui {
        (None, None)
    } else {
        let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(128);
        let ctx = UiContext {
            ticket: ticket_for_ui,
        };
        let handle = tokio::spawn(async move {
            if let Err(e) = ui::run_ui(snapshot_rx, ui_tx, ctx).await {
                warn!(error = %e, "UI task exited with error");
            }
        });
        (Some(ui_rx), Some(handle))
    };

    let result = app.run(events_rx, ui_rx).await;

    if let Some(h) = ui_handle {
        // Best-effort — if the UI task is still waiting on stdin we just
        // drop the handle and let the process exit clean up.
        h.abort();
        let _ = h.await;
    }
    result
}

async fn spawn_mpv(
    binary: Option<PathBuf>,
    initial_file: Option<PathBuf>,
) -> Result<(MpvHandle, mpsc::Receiver<syncmesh_player::MpvEvent>)> {
    let mut opts = SpawnOptions::default();
    if let Some(b) = binary {
        opts.mpv_binary = b;
    }
    opts.initial_file = initial_file;
    let (handle, events) = mpv_spawn(opts).await.context("spawning mpv")?;
    Ok((handle, events))
}

fn print_splash() {
    println!("syncmesh — P2P Syncplay alternative for mpv");
    println!();
    println!("USAGE:");
    println!("  syncmesh create [--file PATH]          start a room, print a ticket to share");
    println!("  syncmesh join <TICKET> [--file PATH]   join an existing room");
    println!();
    println!("Run `syncmesh --help` for all options.");
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("SYNCMESH_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,syncmesh=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .init();
}
