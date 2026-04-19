//! syncmesh CLI entry point.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use syncmesh_core::RoomState;
use syncmesh_net::{
    MeshConfig, MeshEndpoint, PeerLink, decode_ticket, endpoint_id_to_node, identity,
};
use syncmesh_player::{MpvHandle, SpawnOptions, connect as mpv_connect, spawn as mpv_spawn};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, prelude::*};

use syncmesh::app::{App, LoopEvent};
use syncmesh::cli::{Cli, Command};
use syncmesh::config::{Config, MpvSpawn, Paths};
use syncmesh::peer_task;
use syncmesh::ui::{self, UiContext, UiEvent};

const EVENT_QUEUE: usize = 1024;
/// Well-known IPC path used by the bundled `scripts/syncmesh.lua` script.
/// The script opens the socket here; `syncmesh --no-spawn` connects to it.
#[cfg(unix)]
const SCRIPT_IPC_PATH: &str = "/tmp/syncmesh-mpv.sock";
#[cfg(windows)]
const SCRIPT_IPC_PATH: &str = r"\\.\pipe\syncmesh-mpv";

/// Resolved runtime settings after merging CLI flags with the TOML config.
/// CLI beats file; file beats built-in default.
struct Settings {
    nickname: Option<String>,
    mpv_binary: Option<PathBuf>,
    mpv_spawn: MpvSpawn,
    relay: Option<String>,
    log_level: String,
    log_file: Option<PathBuf>,
    verbose: bool,
    qr: bool,
    paths: Paths,
    identity_path: PathBuf,
}

impl Settings {
    fn merge(cli: &Cli, config: Config, paths: Paths) -> Self {
        let identity_path = config.identity_path(&paths);
        let mpv_spawn = if cli.no_mpv {
            MpvSpawn::Disabled
        } else if cli.no_spawn {
            MpvSpawn::Script
        } else {
            config.mpv_spawn.unwrap_or_default()
        };
        let nickname = cli.nickname.clone().or(config.nickname);
        let mpv_binary = cli.mpv_binary.clone().or(config.mpv_binary);
        Self {
            nickname,
            mpv_binary,
            mpv_spawn,
            relay: config.relay.filter(|s| !s.is_empty()),
            log_level: config.log_level.unwrap_or_else(|| "info".to_string()),
            log_file: cli.log_file.clone(),
            verbose: cli.verbose,
            qr: cli.qr || config.qr.unwrap_or(false),
            paths,
            identity_path,
        }
    }

    fn mesh_config(&self) -> Result<MeshConfig> {
        let mut cfg = MeshConfig::default();
        if let Some(url) = self.relay.as_ref() {
            let parsed = url
                .parse()
                .with_context(|| format!("parsing relay url `{url}`"))?;
            cfg = cfg.with_relay_override(parsed);
        }
        Ok(cfg)
    }
}

#[tokio::main(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let Some(command) = cli.command.clone() else {
        init_tracing_minimal();
        print_splash();
        return Ok(());
    };

    let paths = Paths::discover()?;
    let config = Config::load_or_write_template(&paths.config_file)?;
    let settings = Settings::merge(&cli, config, paths);
    // Tracing has to be set up before any `info!` fires; keep the guard alive
    // until the end of main so the rolling file appender flushes on drop.
    let _tracing_guard = init_tracing(&settings)?;

    info!(
        config_dir = %settings.paths.config_dir.display(),
        config_file = %settings.paths.config_file.display(),
        "config loaded"
    );
    let secret = identity::load_or_create(&settings.identity_path)
        .context("loading or generating identity")?;

    let mesh_config = settings.mesh_config()?;
    if settings.relay.is_some() {
        info!(relay = %settings.relay.as_deref().unwrap_or(""), "using self-hosted relay override");
    }
    let mesh = MeshEndpoint::bind(secret, mesh_config)
        .await
        .context("binding mesh endpoint")?;
    let local_node = endpoint_id_to_node(mesh.endpoint_id());
    let local_nickname = settings
        .nickname
        .clone()
        .unwrap_or_else(|| format!("peer-{local_node:?}"));

    // Launch mpv if requested.
    let (mpv_handle, mpv_events) = match settings.mpv_spawn {
        MpvSpawn::Disabled => (None, None),
        MpvSpawn::Auto => {
            let file = match &command {
                Command::Create { file } | Command::Join { file, .. } => file.clone(),
            };
            let (h, rx) = spawn_mpv(settings.mpv_binary.clone(), file).await?;
            (Some(h), Some(rx))
        }
        MpvSpawn::Script => {
            let (h, rx) = connect_script_mpv().await?;
            (Some(h), Some(rx))
        }
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
                if settings.qr {
                    print_qr(&ticket);
                }
            } else {
                info!(%ticket, "room ticket (press `c` in UI to copy)");
                if settings.qr {
                    print_qr(&ticket);
                }
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

async fn connect_script_mpv() -> Result<(MpvHandle, mpsc::Receiver<syncmesh_player::MpvEvent>)> {
    let path = Path::new(SCRIPT_IPC_PATH);
    info!(
        ?path,
        "connecting to user-launched mpv — load scripts/syncmesh.lua before running this command"
    );
    let (handle, events) = mpv_connect(path, std::time::Duration::from_secs(5))
        .await
        .with_context(|| format!("connecting to mpv IPC at {}", path.display()))?;
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

/// Stderr-only tracing used for the splash/`--help` exit paths where we
/// haven't read config yet.
fn init_tracing_minimal() {
    let filter = EnvFilter::try_from_env("SYNCMESH_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,syncmesh=debug"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .try_init();
}

/// Set up stderr + optional JSON log-file sinks.
///
/// Returns a [`tracing_appender::non_blocking::WorkerGuard`] when `--log-file`
/// is set; the caller must keep it alive for the duration of the process so
/// the appender flushes on shutdown.
fn init_tracing(
    settings: &Settings,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    // Precedence for stderr filter: --verbose > SYNCMESH_LOG env > config.log_level > default.
    let stderr_filter = if settings.verbose {
        EnvFilter::new("debug,syncmesh=trace")
    } else if let Ok(env) = EnvFilter::try_from_env("SYNCMESH_LOG") {
        env
    } else {
        EnvFilter::new(format!("{lvl},syncmesh=debug", lvl = settings.log_level))
    };

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_level(true)
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter);

    if let Some(path) = settings.log_file.as_ref() {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent for log-file {}", parent.display())
                })?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening log-file {}", path.display()))?;
        let (writer, guard) = tracing_appender::non_blocking(file);
        let file_filter = EnvFilter::new(format!("{lvl},syncmesh=debug", lvl = settings.log_level));
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_filter(file_filter);
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .try_init()
            .ok();
        Ok(Some(guard))
    } else {
        tracing_subscriber::registry()
            .with(stderr_layer)
            .try_init()
            .ok();
        Ok(None)
    }
}

#[cfg(feature = "qr")]
fn print_qr(ticket: &str) {
    use qrcode::{EcLevel, QrCode, render::unicode::Dense1x2};
    match QrCode::with_error_correction_level(ticket.as_bytes(), EcLevel::M) {
        Ok(code) => {
            let image = code
                .render::<Dense1x2>()
                .dark_color(Dense1x2::Light)
                .light_color(Dense1x2::Dark)
                .build();
            println!("{image}");
        }
        Err(e) => warn!(error = %e, "failed to render QR code"),
    }
}

#[cfg(not(feature = "qr"))]
fn print_qr(_ticket: &str) {
    warn!("QR output requested but this build was compiled without the `qr` feature");
}
