//! The JSON IPC client: turns an [`IpcTransport`] into a stream of
//! [`MpvEvent`]s and a sink that accepts [`MpvCommand`]s.
//!
//! mpv's IPC protocol is line-delimited JSON: each line is either a command
//! response (tagged with `request_id`) or an asynchronous event (tagged with
//! `event`). We run one reader task and one writer task; the public API is a
//! pair of mpsc channels.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

use crate::command::MpvCommand;
use crate::error::MpvError;
use crate::event::{MpvEvent, translate};
use crate::transport::IpcTransport;

/// Properties we ask mpv to `observe_property` on. Exactly the set in plan
/// decision 18.
pub const OBSERVED_PROPERTIES: &[&str] = &[
    "pause",
    "time-pos",
    "seeking",
    "playback-restart", // pseudo-property: subscribed by event, see below
    "eof-reached",
    "speed",
    "filename",
    "duration",
    "file-size",
];

/// Properties we can actually `observe_property` on. `playback-restart` is
/// already delivered as a top-level event by mpv, so we don't observe it.
const OBSERVABLE_PROPERTIES: &[&str] = &[
    "pause",
    "time-pos",
    "seeking",
    "eof-reached",
    "speed",
    "filename",
    "duration",
    "file-size",
];

type ReplyMap = Arc<Mutex<HashMap<u64, oneshot::Sender<CommandReply>>>>;

#[derive(Debug)]
struct CommandReply {
    error: String,
    _data: serde_json::Value,
}

/// The running IPC client. Holds the writer half and a handle to the reader
/// task; drop it to close the connection.
pub struct IpcClient {
    request_seq: Arc<AtomicU64>,
    writer_tx: mpsc::Sender<WriterMsg>,
    pending: ReplyMap,
    /// Joined on shutdown. Returns `Ok(())` if the reader drained cleanly.
    reader_task: JoinHandle<Result<(), MpvError>>,
    writer_task: JoinHandle<Result<(), MpvError>>,
}

impl std::fmt::Debug for IpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcClient").finish_non_exhaustive()
    }
}

/// Messages pushed into the writer task.
#[derive(Debug)]
enum WriterMsg {
    Line(String),
    Shutdown,
}

impl IpcClient {
    /// Split the transport into reader/writer, spawn both tasks, subscribe to
    /// observed properties, and return the client + an event stream.
    pub async fn start(
        transport: Box<dyn IpcTransport>,
    ) -> Result<(Self, mpsc::Receiver<MpvEvent>), MpvError> {
        let (reader_half, writer_half) = tokio::io::split(transport);
        let (event_tx, event_rx) = mpsc::channel(128);
        let (writer_tx, writer_rx) = mpsc::channel::<WriterMsg>(64);
        let pending: ReplyMap = Arc::new(Mutex::new(HashMap::new()));

        let pending_for_reader = pending.clone();
        let reader_task = tokio::spawn(async move {
            read_loop(reader_half, event_tx, pending_for_reader).await
        });
        let writer_task = tokio::spawn(async move { write_loop(writer_half, writer_rx).await });

        let client = Self {
            request_seq: Arc::new(AtomicU64::new(1)),
            writer_tx,
            pending,
            reader_task,
            writer_task,
        };

        // Subscribe to the properties the rest of the app cares about.
        for (id, name) in OBSERVABLE_PROPERTIES.iter().enumerate() {
            let request_id = client.request_seq.fetch_add(1, Ordering::Relaxed);
            let frame = serde_json::json!({
                "command": ["observe_property", (id + 1) as u64, name],
                "request_id": request_id,
            });
            client.send_raw(frame, Some(request_id)).await?;
        }

        Ok((client, event_rx))
    }

    /// Send an [`MpvCommand`] and await mpv's ack. Returns `Ok` on
    /// `error: "success"`; surfaces mpv's reported error string otherwise.
    pub async fn send(&self, cmd: &MpvCommand) -> Result<(), MpvError> {
        let request_id = self.request_seq.fetch_add(1, Ordering::Relaxed);
        let frame = cmd.to_json(request_id);
        self.send_raw(frame, Some(request_id)).await
    }

    /// Fire-and-forget: send a frame without waiting for the reply. Used for
    /// the IPC `quit` command during shutdown, where mpv may close the pipe
    /// before the reply is written.
    pub async fn send_no_wait(&self, cmd: &MpvCommand) -> Result<(), MpvError> {
        let request_id = self.request_seq.fetch_add(1, Ordering::Relaxed);
        let frame = cmd.to_json(request_id);
        self.send_raw(frame, None).await
    }

    async fn send_raw(
        &self,
        frame: serde_json::Value,
        await_request_id: Option<u64>,
    ) -> Result<(), MpvError> {
        let line = format!("{frame}\n");
        let reply_rx = if let Some(id) = await_request_id {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(id, tx);
            Some(rx)
        } else {
            None
        };
        self.writer_tx
            .send(WriterMsg::Line(line))
            .await
            .map_err(|_| MpvError::Closed)?;
        if let Some(rx) = reply_rx {
            match rx.await {
                Ok(reply) if reply.error == "success" => Ok(()),
                Ok(reply) => Err(MpvError::CommandFailed(reply.error)),
                // Reader died before answering — map to a stable variant.
                Err(_) => Err(MpvError::Closed),
            }
        } else {
            Ok(())
        }
    }

    /// Send a raw `quit` command and wait briefly for mpv to close.
    pub async fn quit(&self) -> Result<(), MpvError> {
        let request_id = self.request_seq.fetch_add(1, Ordering::Relaxed);
        let frame = serde_json::json!({
            "command": ["quit"],
            "request_id": request_id,
        });
        // Don't await a reply — mpv closes the pipe before it writes one.
        self.send_raw(frame, None).await
    }

    /// Shut down the reader + writer tasks. Called from [`Drop`] as best-effort,
    /// and from [`crate::process::MpvHandle::shutdown`] explicitly.
    pub async fn shutdown(self) -> Result<(), MpvError> {
        let _ = self.writer_tx.send(WriterMsg::Shutdown).await;
        let _ = self.writer_task.await;
        // Draining the reader can block on the OS; abort it.
        self.reader_task.abort();
        Ok(())
    }
}

async fn read_loop<R>(
    reader: R,
    events: mpsc::Sender<MpvEvent>,
    pending: ReplyMap,
) -> Result<(), MpvError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => {
                // mpv closed the pipe — normal shutdown.
                let _ = events.send(MpvEvent::Shutdown).await;
                return Ok(());
            }
            Err(e) => {
                let _ = events.send(MpvEvent::Shutdown).await;
                return Err(MpvError::Io(e));
            }
        };
        if line.is_empty() {
            continue;
        }
        trace!(%line, "mpv -> us");
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(?e, %line, "ignoring malformed mpv frame");
                continue;
            }
        };
        // Command reply?
        if let Some(obj) = value.as_object() {
            if let Some(id) = obj.get("request_id").and_then(|v| v.as_u64()) {
                let error = obj
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("success")
                    .to_owned();
                let data = obj.get("data").cloned().unwrap_or(serde_json::Value::Null);
                let mut map = pending.lock().await;
                if let Some(waiter) = map.remove(&id) {
                    let _ = waiter.send(CommandReply { error, _data: data });
                }
                // Replies that nobody awaited (send_no_wait, or
                // observe_property whose sender was dropped) fall through
                // silently — they're not events.
                continue;
            }
        }
        // Event path.
        if let Some(event) = translate(&value) {
            if events.send(event).await.is_err() {
                debug!("event consumer dropped; ending reader");
                return Ok(());
            }
        }
    }
}

async fn write_loop<W>(mut writer: W, mut rx: mpsc::Receiver<WriterMsg>) -> Result<(), MpvError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(msg) = rx.recv().await {
        match msg {
            WriterMsg::Line(s) => {
                trace!(line = %s.trim_end(), "us -> mpv");
                if let Err(e) = writer.write_all(s.as_bytes()).await {
                    // If mpv died mid-write, surface the error but don't loop.
                    return Err(MpvError::Io(e));
                }
                if let Err(e) = writer.flush().await {
                    return Err(MpvError::Io(e));
                }
            }
            WriterMsg::Shutdown => break,
        }
    }
    let _ = writer.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A minimal fake mpv: reads command lines, replies with success, and
    /// optionally emits a preloaded event stream.
    async fn run_fake_mpv(
        mut stream: tokio::io::DuplexStream,
        emit_events: Vec<serde_json::Value>,
    ) {
        // Split manually so we can read and write concurrently.
        let (r, mut w) = tokio::io::split(&mut stream);
        let mut lines = BufReader::new(r).lines();
        // Push events first so the client sees them immediately.
        for ev in &emit_events {
            let line = format!("{ev}\n");
            let _ = w.write_all(line.as_bytes()).await;
        }
        let _ = w.flush().await;
        while let Ok(Some(line)) = lines.next_line().await {
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = v
                .as_object()
                .and_then(|o| o.get("request_id"))
                .and_then(|v| v.as_u64());
            if let Some(id) = id {
                let reply =
                    serde_json::json!({"request_id": id, "error": "success", "data": null});
                let line = format!("{reply}\n");
                let _ = w.write_all(line.as_bytes()).await;
                let _ = w.flush().await;
            }
        }
    }

    #[tokio::test]
    async fn start_subscribes_to_all_observable_properties() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        // Spy: record every line the client sends.
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server_side);
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = v
                    .get("request_id")
                    .and_then(|v| v.as_u64())
                    .expect("must have request_id");
                let reply =
                    serde_json::json!({"request_id": id, "error": "success", "data": null});
                let line_out = format!("{reply}\n");
                let _ = w.write_all(line_out.as_bytes()).await;
                let _ = w.flush().await;
                let _ = tx.send(v).await;
            }
        });
        let (client, _events) = IpcClient::start(Box::new(client_side)).await.unwrap();
        // Should have seen observe_property for each property.
        let mut observed: Vec<String> = Vec::new();
        for _ in 0..OBSERVABLE_PROPERTIES.len() {
            let v = rx.recv().await.unwrap();
            let cmd = v.get("command").unwrap().as_array().unwrap();
            assert_eq!(cmd[0].as_str(), Some("observe_property"));
            observed.push(cmd[2].as_str().unwrap().to_string());
        }
        let mut expected: Vec<String> = OBSERVABLE_PROPERTIES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        observed.sort();
        expected.sort();
        assert_eq!(observed, expected);
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn send_pause_awaits_success() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(run_fake_mpv(server_side, vec![]));
        let (client, _events) = IpcClient::start(Box::new(client_side)).await.unwrap();
        client.send(&MpvCommand::Pause(true)).await.unwrap();
        client.send(&MpvCommand::Seek { media_pos_ms: 5_000 }).await.unwrap();
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn send_surfaces_mpv_error() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        // Custom fake: replies with error for the pause command (request_id
        // greater than the observe_property count means it's the user's).
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server_side);
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = v.get("request_id").and_then(|v| v.as_u64()).unwrap();
                let cmd = v.get("command").unwrap().as_array().unwrap();
                let first = cmd[0].as_str().unwrap();
                let reply = if first == "observe_property" {
                    serde_json::json!({"request_id": id, "error": "success"})
                } else {
                    serde_json::json!({"request_id": id, "error": "invalid parameter"})
                };
                let line_out = format!("{reply}\n");
                let _ = w.write_all(line_out.as_bytes()).await;
                let _ = w.flush().await;
            }
        });
        let (client, _events) = IpcClient::start(Box::new(client_side)).await.unwrap();
        let r = client.send(&MpvCommand::Pause(true)).await;
        assert!(matches!(r, Err(MpvError::CommandFailed(_))));
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn inbound_events_are_translated_and_delivered() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let events = vec![
            serde_json::json!({"event": "property-change", "name": "pause", "data": true}),
            serde_json::json!({"event": "property-change", "name": "time-pos", "data": 5.5}),
            serde_json::json!({"event": "seek"}),
            serde_json::json!({"event": "playback-restart"}),
            serde_json::json!({"event": "property-change", "name": "duration", "data": 3600.0}),
            serde_json::json!({"event": "property-change", "name": "filename", "data": "m.mkv"}),
        ];
        tokio::spawn(run_fake_mpv(server_side, events));
        let (client, mut event_rx) = IpcClient::start(Box::new(client_side)).await.unwrap();

        let mut got: Vec<MpvEvent> = Vec::new();
        for _ in 0..6 {
            got.push(
                tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv())
                    .await
                    .expect("event must arrive")
                    .expect("channel must not close"),
            );
        }
        assert!(got.contains(&MpvEvent::Pause(true)));
        assert!(got.contains(&MpvEvent::TimePos(5.5)));
        assert!(got.contains(&MpvEvent::Seeking));
        assert!(got.contains(&MpvEvent::PlaybackRestart));
        assert!(got.contains(&MpvEvent::Duration(3600.0)));
        assert!(got.contains(&MpvEvent::Filename("m.mkv".to_string())));
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn transport_close_emits_shutdown_event() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        // Drop the server after observe_property handshake.
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server_side);
            let mut lines = BufReader::new(r).lines();
            for _ in 0..OBSERVABLE_PROPERTIES.len() {
                if let Ok(Some(line)) = lines.next_line().await {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let id = v.get("request_id").and_then(|v| v.as_u64()).unwrap();
                    let reply = serde_json::json!({"request_id": id, "error": "success"});
                    let line_out = format!("{reply}\n");
                    let _ = w.write_all(line_out.as_bytes()).await;
                    let _ = w.flush().await;
                }
            }
            drop(w);
        });
        let (client, mut events) = IpcClient::start(Box::new(client_side)).await.unwrap();
        // Eventually Shutdown arrives.
        let shutdown = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match events.recv().await {
                    Some(MpvEvent::Shutdown) => return true,
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await
        .unwrap();
        assert!(shutdown);
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn garbled_frames_are_skipped() {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server_side);
            let mut lines = BufReader::new(r).lines();
            // Inject garbage, then a valid event, then answer handshakes.
            let _ = w.write_all(b"not json at all\n").await;
            let _ = w
                .write_all(b"{\"event\":\"property-change\",\"name\":\"pause\",\"data\":false}\n")
                .await;
            let _ = w.flush().await;
            while let Ok(Some(line)) = lines.next_line().await {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = v.get("request_id").and_then(|v| v.as_u64()).unwrap();
                let reply = serde_json::json!({"request_id": id, "error": "success"});
                let _ = w
                    .write_all(format!("{reply}\n").as_bytes())
                    .await;
                let _ = w.flush().await;
            }
        });
        let (client, mut events) = IpcClient::start(Box::new(client_side)).await.unwrap();
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, MpvEvent::Pause(false));
        let _ = client.shutdown().await;
    }
}
