//! Spawning the mpv child process and tying its lifecycle to the IPC client.
//!
//! The "default" integration mode from plan decision 16: we launch mpv with
//! `--input-ipc-server=<path>`, connect over Unix socket / Windows named pipe
//! once mpv has created the endpoint, and expose a [`MpvHandle`] that sends
//! commands and receives events. Dropping the handle kills mpv.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::command::MpvCommand;
use crate::error::{MpvError, MpvExit};
use crate::event::MpvEvent;
use crate::ipc::IpcClient;
use crate::transport::{connect_transport, generate_ipc_path};

/// Options governing how mpv is spawned.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Path to the mpv binary. Defaults to `"mpv"` (PATH lookup).
    pub mpv_binary: PathBuf,
    /// The IPC endpoint to use. If `None`, a unique path is generated.
    pub ipc_path: Option<PathBuf>,
    /// File to open on startup. `None` starts mpv idle.
    pub initial_file: Option<PathBuf>,
    /// Extra flags to pass to mpv, in addition to the IPC ones.
    pub extra_args: Vec<OsString>,
    /// How long to wait for the IPC socket/pipe to appear before giving up.
    pub connect_timeout: Duration,
    /// If true, pass `--idle=yes --force-window=yes` so mpv stays alive even
    /// with no file. Recommended for interactive syncmesh use.
    pub keep_alive: bool,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            mpv_binary: PathBuf::from("mpv"),
            ipc_path: None,
            initial_file: None,
            extra_args: Vec::new(),
            connect_timeout: Duration::from_secs(5),
            keep_alive: true,
        }
    }
}

/// A running mpv instance with its IPC client.
///
/// Drop order: the handle holds the `Child` behind a shared mutex; the
/// background watcher task also holds a reference and calls `wait` on it.
/// When `shutdown` runs, we gracefully quit mpv, wait briefly, and then kill
/// the child if needed. If the handle is just dropped, the `kill_on_drop`
/// flag on the `Child` ensures mpv is torn down.
pub struct MpvHandle {
    client: Option<IpcClient>,
    exit_rx: Option<oneshot::Receiver<MpvExit>>,
    child: SharedChild,
}

impl std::fmt::Debug for MpvHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpvHandle").finish_non_exhaustive()
    }
}

type SharedChild = Arc<StdMutex<Option<Child>>>;

impl MpvHandle {
    /// Send a command and wait for mpv's ack.
    pub async fn send(&self, cmd: MpvCommand) -> Result<(), MpvError> {
        let client = self.client.as_ref().ok_or(MpvError::Closed)?;
        client.send(&cmd).await
    }

    /// Send a [`syncmesh_core::MpvCommand`] straight through — the state
    /// machine's output type.
    pub async fn send_core(&self, cmd: syncmesh_core::MpvCommand) -> Result<(), MpvError> {
        self.send(cmd.into()).await
    }

    /// Gracefully ask mpv to quit, then wait for the child to exit. Returns
    /// how it ended.
    pub async fn shutdown(mut self) -> MpvExit {
        if let Some(client) = self.client.take() {
            let _ = client.quit().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = client.shutdown().await;
        }
        if let Some(exit_rx) = self.exit_rx.take() {
            if let Ok(Ok(reason)) = tokio::time::timeout(Duration::from_secs(2), exit_rx).await {
                return reason;
            }
            warn!("mpv did not exit after quit; killing");
        }
        self.force_kill();
        MpvExit::Killed
    }

    fn force_kill(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

impl Drop for MpvHandle {
    fn drop(&mut self) {
        // If the user forgot to call shutdown, make sure mpv doesn't outlive
        // us. `kill_on_drop(true)` on the Child would suffice, but we also
        // set it explicitly in case the watcher already took the Child.
        self.force_kill();
    }
}

/// Spawn mpv, connect over IPC, and return a handle + event stream.
pub async fn spawn(
    options: SpawnOptions,
) -> Result<(MpvHandle, mpsc::Receiver<MpvEvent>), MpvError> {
    let ipc_path = options.ipc_path.clone().unwrap_or_else(generate_ipc_path);

    let mut cmd = Command::new(&options.mpv_binary);
    cmd.arg(format!(
        "--input-ipc-server={}",
        ipc_path_to_arg(&ipc_path)
    ));
    cmd.arg("--no-terminal");
    if options.keep_alive {
        cmd.arg("--idle=yes").arg("--force-window=yes");
    }
    for extra in &options.extra_args {
        cmd.arg(extra);
    }
    if let Some(file) = &options.initial_file {
        cmd.arg(file);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    cmd.kill_on_drop(true);

    info!(?ipc_path, bin = ?options.mpv_binary, "spawning mpv");
    let child = cmd.spawn().map_err(MpvError::Spawn)?;

    let shared: SharedChild = Arc::new(StdMutex::new(Some(child)));
    let (exit_tx, exit_rx) = oneshot::channel();

    // Watcher: periodically try_wait until the child is gone.
    let watcher_child = Arc::clone(&shared);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut guard = match watcher_child.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let status = match guard.as_mut() {
                Some(c) => match c.try_wait() {
                    Ok(Some(s)) => Some(s),
                    Ok(None) => None,
                    Err(_) => None,
                },
                None => {
                    // Someone took the child out. Nothing to report.
                    return;
                }
            };
            if let Some(s) = status {
                *guard = None;
                drop(guard);
                let reason = if s.success() {
                    MpvExit::Clean { code: s.code() }
                } else {
                    MpvExit::Crashed { code: s.code() }
                };
                debug!(?reason, "mpv process ended");
                let _ = exit_tx.send(reason);
                return;
            }
        }
    });

    // Connect to the IPC endpoint.
    let transport = match connect_transport(&ipc_path, options.connect_timeout).await {
        Ok(t) => t,
        Err(e) => {
            // Kill the child we just spawned; nothing will ever talk to it.
            if let Ok(mut g) = shared.lock() {
                if let Some(c) = g.as_mut() {
                    let _ = c.start_kill();
                }
            }
            return Err(e);
        }
    };
    let (client, events) = IpcClient::start(transport).await?;

    Ok((
        MpvHandle {
            client: Some(client),
            exit_rx: Some(exit_rx),
            child: shared,
        },
        events,
    ))
}

fn ipc_path_to_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
