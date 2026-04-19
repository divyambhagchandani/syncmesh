//! The syncmesh event loop.
//!
//! One `tokio::select!` owns the authoritative `RoomState`. Every input (mpv
//! event, inbound frame, accepted peer, tick, Ctrl-C) funnels through a single
//! mpsc and is fed to `RoomState::apply`. The returned `Output`s are dispatched
//! to mpv or to per-peer writer channels.
//!
//! ## Local mpv edges → broadcast
//!
//! mpv edges arrive on three kinds of paths:
//!
//! 1. **User-initiated**: the user hit `space` in mpv, dragged the timeline,
//!    or changed speed. We want to broadcast these to the mesh as
//!    `LocalControl` so every other peer follows along.
//! 2. **Echo of a command we just sent**: the state machine applied an inbound
//!    `ControlEvent`, dispatched the corresponding `MpvCommand`, and mpv now
//!    reports the property change back at us. Broadcasting this would cause
//!    an event loop around the mesh.
//! 3. **Passive playback progress**: `TimePos` ticks every ~100 ms. These are
//!    not user edges; they only update the local playback snapshot used by
//!    heartbeats and drift.
//!
//! We distinguish (1) from (2) with the `EchoGuard`: every time we dispatch an
//! outbound `MpvCommand` we arm a one-shot suppression for the matching edge.
//! We distinguish seek edges from passive `TimePos` ticks using mpv's own
//! `Seeking` → `PlaybackRestart` sequence: the first `TimePos` after that pair
//! is the seek target, and any `TimePos` outside that window is passive.
//!
//! ## N>2 full mesh
//!
//! When peer C joins a room that already contains {A, B}, C dials B (or A),
//! receives a `PresenceEvent::PeerList` from them, and must dial every other
//! peer listed so it forms a direct link (decision 4/8). The `PeerList`'s third
//! tuple element is the opaque postcard-encoded `EndpointAddr` for each peer;
//! the core crate treats it as bytes, this crate decodes and dials.
//!
//! Addresses flow into the registry through two paths:
//!
//! 1. **Outbound dials** — when we dial a peer (either via ticket or from a
//!    `PeerList` entry), we know that peer's `EndpointAddr` and record it.
//! 2. **`AddrAnnounce` broadcasts** — on connect, every peer announces its
//!    current `EndpointAddr` to the mesh, so peers we accepted (where iroh
//!    doesn't surface the caller's full address) still become dialable.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use syncmesh_core::{
    ControlAction, Frame, Input, MpvCommand as CoreMpvCommand, NodeId, Notice, Output, PresenceEvent,
    RoomSnapshot, RoomState,
};
use syncmesh_net::{MeshEndpoint, PeerLink};
use syncmesh_player::{MpvEvent, MpvHandle};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, interval_at};
use tracing::{debug, info, warn};

use crate::addrs::{self, AddrRegistry};
use crate::echo::EchoGuard;
use crate::media::MediaCollector;
use crate::peer_task;
use crate::ui::UiEvent;

/// How often we fan out our own heartbeat and run drift correction.
const TICK_PERIOD: Duration = Duration::from_secs(1);

/// Inbound events to the event loop.
#[derive(Debug)]
pub enum LoopEvent {
    Mpv(MpvEvent),
    /// A decoded frame arrived from a peer.
    Peer { from: NodeId, frame: Frame },
    /// Either an accepted incoming connection or a freshly dialed outbound
    /// one. `addr_bytes` is `Some` when we initiated the dial (we already have
    /// the remote's `EndpointAddr`); `None` when we accepted (iroh doesn't
    /// surface the caller's full address, we'll learn it via `AddrAnnounce`).
    PeerConnected {
        link: PeerLink,
        addr_bytes: Option<Vec<u8>>,
    },
    /// A dial task we spawned failed; clear the pending-dials slot.
    PeerDialFailed { node: NodeId },
    PeerDisconnected { node: NodeId },
    /// User pressed Ctrl-C.
    Shutdown,
}

/// Per-peer state owned by the event loop.
struct LivePeer {
    link: PeerLink,
    writer: mpsc::Sender<Frame>,
}

pub struct App {
    state: RoomState,
    mesh: MeshEndpoint,
    mpv: Option<MpvHandle>,
    peers: HashMap<NodeId, LivePeer>,
    events_tx: mpsc::Sender<LoopEvent>,
    snapshot_tx: watch::Sender<RoomSnapshot>,
    echo: EchoGuard,
    media: MediaCollector,
    /// Our own `NodeId`. Cached so we can cheaply filter self-entries out of
    /// inbound `PeerList`s and rewrite outgoing `PeerList`s with our own
    /// address.
    local: NodeId,
    /// `NodeId -> postcard(EndpointAddr)` map used for dialing peers learned
    /// via `PeerList` and for filling the address field of outgoing
    /// `PeerList` frames.
    addrs: AddrRegistry,
    /// Nodes we've spawned a dial task for but haven't yet observed as
    /// connected. Prevents duplicate dials on repeated `PeerList` frames.
    pending_dials: HashSet<NodeId>,
    /// Set when mpv emits `Seeking`; cleared on `PlaybackRestart`. Governs
    /// whether the next `TimePos` is a seek target or a passive update.
    seek_in_progress: bool,
    /// Set on `PlaybackRestart` if a seek was in progress; the next `TimePos`
    /// is the seek target and must be evaluated for local broadcast.
    seek_just_completed: bool,
}

// Hand-rolled because `MeshEndpoint`, `MpvHandle`, `PeerLink`, and the mpsc /
// watch channel ends intentionally don't implement `Debug`. Skip those and
// surface a connection-count summary instead of the full peer map.
impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("state", &self.state)
            .field("local", &self.local)
            .field("mpv_spawned", &self.mpv.is_some())
            .field("peer_count", &self.peers.len())
            .field("pending_dials", &self.pending_dials)
            .field("addrs", &self.addrs)
            .field("echo", &self.echo)
            .field("media", &self.media)
            .field("seek_in_progress", &self.seek_in_progress)
            .field("seek_just_completed", &self.seek_just_completed)
            .finish_non_exhaustive()
    }
}

impl App {
    pub fn new(
        state: RoomState,
        mesh: MeshEndpoint,
        mpv: Option<MpvHandle>,
        events_tx: mpsc::Sender<LoopEvent>,
    ) -> (Self, watch::Receiver<RoomSnapshot>) {
        let local = state.local_node();
        let addrs = AddrRegistry::with_self(&mesh);
        let (snapshot_tx, snapshot_rx) = watch::channel(state.snapshot());
        let app = Self {
            state,
            mesh,
            mpv,
            peers: HashMap::new(),
            events_tx,
            snapshot_tx,
            echo: EchoGuard::new(),
            media: MediaCollector::new(),
            local,
            addrs,
            pending_dials: HashSet::new(),
            seek_in_progress: false,
            seek_just_completed: false,
        };
        (app, snapshot_rx)
    }

    /// Record a peer's `EndpointAddr` bytes. Used by `main` right after it
    /// decodes a ticket so the first `PeerList` we send back already has the
    /// host's address populated.
    pub fn record_addr(&mut self, node: NodeId, bytes: Vec<u8>) {
        self.addrs.insert(node, bytes);
    }

    /// Drive the loop until `Shutdown` is received.
    ///
    /// If `ui_events` is `Some`, user-intent events from the TUI are folded
    /// into the same select! so they arbitrate against network/mpv events
    /// with the same priority. When `None` (used by the headless `--no-ui`
    /// mode that ships in v0.2), the UI branch is simply never polled.
    pub async fn run(
        mut self,
        mut events: mpsc::Receiver<LoopEvent>,
        mut ui_events: Option<mpsc::Receiver<UiEvent>>,
    ) -> Result<()> {
        let tick_start = Instant::now() + TICK_PERIOD;
        let mut ticker = interval_at(tick_start, TICK_PERIOD);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.on_tick().await;
                }
                maybe = events.recv() => {
                    let Some(ev) = maybe else {
                        debug!("event channel closed; exiting loop");
                        break;
                    };
                    match ev {
                        LoopEvent::Shutdown => {
                            info!("shutdown requested");
                            break;
                        }
                        LoopEvent::Mpv(mpv_ev) => self.on_mpv(mpv_ev).await,
                        LoopEvent::Peer { from, frame } => {
                            self.on_peer_frame(from, frame).await;
                        }
                        LoopEvent::PeerConnected { link, addr_bytes } => {
                            self.on_peer_connected(link, addr_bytes).await;
                        }
                        LoopEvent::PeerDialFailed { node } => {
                            self.pending_dials.remove(&node);
                        }
                        LoopEvent::PeerDisconnected { node } => {
                            self.on_peer_disconnected(node).await;
                        }
                    }
                }
                maybe_ui = recv_opt(&mut ui_events) => {
                    match maybe_ui {
                        Some(UiEvent::Quit) => {
                            info!("UI requested quit");
                            break;
                        }
                        Some(ev) => self.on_ui_event(ev).await,
                        None => {
                            // The UI task dropped its sender; keep running
                            // headless (e.g. the terminal was closed but the
                            // user still wants the mesh up until Ctrl-C).
                            ui_events = None;
                        }
                    }
                }
            }
        }

        self.shutdown().await;
        Ok(())
    }

    async fn on_ui_event(&mut self, ev: UiEvent) {
        let now = now_ms();
        match ev {
            UiEvent::ToggleReady => {
                let new_ready = !self.state.local_ready();
                self.apply_and_dispatch(Input::LocalReady {
                    ready: new_ready,
                    now_ms: now,
                })
                .await;
            }
            UiEvent::TogglePauseRelay => {
                let playback = self.state.local_playback();
                let action = if playback.paused {
                    ControlAction::Play {
                        media_pos_ms: playback.media_pos_ms,
                    }
                } else {
                    ControlAction::Pause {
                        media_pos_ms: playback.media_pos_ms,
                    }
                };
                self.apply_and_dispatch(Input::LocalControl {
                    action,
                    now_ms: now,
                })
                .await;
            }
            UiEvent::SubmitChat(text) => {
                if text.is_empty() {
                    return;
                }
                self.apply_and_dispatch(Input::LocalChat { text, now_ms: now })
                    .await;
            }
            UiEvent::ToggleOverride => {
                let was = self.state.ready_gate().is_override_enabled();
                self.apply_and_dispatch(Input::SetOverride { enabled: !was })
                    .await;
            }
            UiEvent::CopyTicket => {
                // Handled entirely in the UI task; the event loop just sees
                // it pass through so we can log for diagnostics.
                debug!("ticket copy requested by UI");
            }
            UiEvent::Quit => {
                // Handled in the main select! arm before dispatch.
            }
        }
    }

    async fn on_tick(&mut self) {
        let now = now_ms();
        let outs = self.state.apply(Input::Tick { now_ms: now });
        self.dispatch(outs).await;
    }

    async fn on_mpv(&mut self, ev: MpvEvent) {
        let now = now_ms();
        match ev {
            MpvEvent::Pause(p) => self.on_mpv_pause(p, now).await,
            MpvEvent::TimePos(secs) => self.on_mpv_time_pos(secs, now).await,
            MpvEvent::Speed(s) => self.on_mpv_speed(s, now).await,
            MpvEvent::Seeking => {
                self.seek_in_progress = true;
            }
            MpvEvent::PlaybackRestart => {
                if self.seek_in_progress {
                    self.seek_just_completed = true;
                    self.seek_in_progress = false;
                }
            }
            MpvEvent::Filename(name) => {
                self.media.on_filename(&name);
                self.maybe_publish_media(now).await;
            }
            MpvEvent::Duration(d) => {
                self.media.on_duration_secs(d);
                self.maybe_publish_media(now).await;
            }
            MpvEvent::FileSize(sz) => {
                self.media.on_file_size(sz);
                self.maybe_publish_media(now).await;
            }
            MpvEvent::EofReached => {
                debug!("mpv reached end of file");
            }
            MpvEvent::Shutdown => {
                warn!("mpv reported shutdown");
            }
        }
    }

    async fn on_mpv_pause(&mut self, paused: bool, now: u64) {
        let playback = self.state.local_playback();
        if self.echo.consume_pause(paused, now) {
            // Echo of a command we dispatched. Refresh local snapshot only.
            self.apply_and_dispatch(Input::MpvStateUpdate {
                media_pos_ms: playback.media_pos_ms,
                paused,
                speed_centi: playback.speed_centi,
            })
            .await;
            return;
        }
        // Genuine user action: broadcast as LocalControl.
        let action = if paused {
            ControlAction::Pause {
                media_pos_ms: playback.media_pos_ms,
            }
        } else {
            ControlAction::Play {
                media_pos_ms: playback.media_pos_ms,
            }
        };
        self.apply_and_dispatch(Input::LocalControl {
            action,
            now_ms: now,
        })
        .await;
    }

    async fn on_mpv_time_pos(&mut self, secs: f64, now: u64) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = (secs.max(0.0) * 1000.0) as u64;
        let playback = self.state.local_playback();
        if self.seek_just_completed {
            self.seek_just_completed = false;
            if self.echo.consume_seek(ms, now) {
                self.apply_and_dispatch(Input::MpvStateUpdate {
                    media_pos_ms: ms,
                    paused: playback.paused,
                    speed_centi: playback.speed_centi,
                })
                .await;
            } else {
                self.apply_and_dispatch(Input::LocalControl {
                    action: ControlAction::Seek { media_pos_ms: ms },
                    now_ms: now,
                })
                .await;
            }
            return;
        }
        // Passive playback progress; keeps heartbeat + drift fresh.
        self.apply_and_dispatch(Input::MpvStateUpdate {
            media_pos_ms: ms,
            paused: playback.paused,
            speed_centi: playback.speed_centi,
        })
        .await;
    }

    async fn on_mpv_speed(&mut self, raw: f64, now: u64) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let centi = (raw * 100.0).round().clamp(0.0, f64::from(u16::MAX)) as u16;
        let playback = self.state.local_playback();
        if self.echo.consume_speed(centi, now) {
            self.apply_and_dispatch(Input::MpvStateUpdate {
                media_pos_ms: playback.media_pos_ms,
                paused: playback.paused,
                speed_centi: centi,
            })
            .await;
            return;
        }
        self.apply_and_dispatch(Input::LocalControl {
            action: ControlAction::SetSpeed {
                speed_centi: centi,
            },
            now_ms: now,
        })
        .await;
    }

    async fn maybe_publish_media(&mut self, now_ms: u64) {
        if let Some(media) = self.media.take_if_changed() {
            info!(?media, "local media changed");
            self.apply_and_dispatch(Input::LocalMediaChanged { media, now_ms })
                .await;
        }
    }

    async fn on_peer_frame(&mut self, from: NodeId, frame: Frame) {
        // Snoop address info before the state machine sees the frame: these
        // are transport-layer concerns the core crate doesn't know about.
        if let Frame::Presence(ref presence) = frame {
            match presence {
                PresenceEvent::AddrAnnounce { node, addr_bytes } => {
                    self.addrs.apply_announce(*node, addr_bytes, self.local);
                }
                PresenceEvent::PeerList { peers } => {
                    for (node, _nick, addr_bytes) in peers {
                        if *node == self.local {
                            continue;
                        }
                        if !addr_bytes.is_empty() {
                            self.addrs.insert(*node, addr_bytes.clone());
                        }
                        if self.peers.contains_key(node) || self.pending_dials.contains(node) {
                            continue;
                        }
                        if addr_bytes.is_empty() {
                            continue;
                        }
                        let Some(addr) = addrs::decode_addr(addr_bytes) else {
                            debug!(peer = ?node, "PeerList entry has undecodable addr; skipping");
                            continue;
                        };
                        self.pending_dials.insert(*node);
                        self.spawn_dial(*node, addr, addr_bytes.clone());
                    }
                }
                _ => {}
            }
        }

        let rtt_sample_ms = self.peers.get(&from).and_then(|p| {
            let mut tmp = syncmesh_core::RttEstimator::new();
            p.link.sample_rtt(&mut tmp)
        });
        let outs = self.state.apply(Input::FrameReceived {
            from,
            frame,
            received_at_ms: now_ms(),
            rtt_sample_ms,
        });
        self.dispatch(outs).await;
    }

    /// Spawn a tokio task that dials `addr`. On success the new link comes
    /// back via `LoopEvent::PeerConnected` carrying the addr bytes so the
    /// main loop can record the authoritative address. On failure we emit
    /// `PeerDialFailed` so `pending_dials` doesn't pin the `NodeId` forever.
    fn spawn_dial(&self, node: NodeId, addr: iroh::EndpointAddr, addr_bytes: Vec<u8>) {
        let mesh = self.mesh.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            match mesh.dial(addr).await {
                Ok(link) => {
                    info!(peer = ?node, "dialed PeerList-discovered peer");
                    let _ = tx
                        .send(LoopEvent::PeerConnected {
                            link,
                            addr_bytes: Some(addr_bytes),
                        })
                        .await;
                }
                Err(e) => {
                    warn!(peer = ?node, error = %e, "transitive dial failed");
                    let _ = tx.send(LoopEvent::PeerDialFailed { node }).await;
                }
            }
        });
    }

    async fn on_peer_connected(&mut self, link: PeerLink, addr_bytes: Option<Vec<u8>>) {
        let node = link.remote();
        self.pending_dials.remove(&node);
        if self.peers.contains_key(&node) {
            debug!(peer = ?node, "duplicate PeerConnected; ignoring");
            return;
        }
        if let Some(bytes) = addr_bytes {
            self.addrs.insert(node, bytes);
        }

        let writer = peer_task::spawn_peer_tasks(&link, &self.events_tx);
        self.peers.insert(node, LivePeer { link, writer });

        // Announce our own address to the new peer. Every other already-
        // connected peer has it (or will, once they receive this peer's own
        // AddrAnnounce). Done *before* `state.apply` so a simultaneously
        // flushed PeerList has the addresses it needs.
        self.addrs.refresh_self(&self.mesh);
        if let Some(self_bytes) = self.addrs.get(&self.local).map(<[u8]>::to_vec) {
            let announce = Frame::Presence(PresenceEvent::AddrAnnounce {
                node: self.local,
                addr_bytes: self_bytes,
            });
            self.send_to(node, announce).await;
        }

        // Use the NodeId's hex as a provisional nickname; a later PresenceEvent
        // will rename the peer.
        let outs = self.state.apply(Input::PeerConnected {
            node,
            nickname: format!("{node:?}"),
        });
        self.dispatch(outs).await;
    }

    async fn on_peer_disconnected(&mut self, node: NodeId) {
        if self.peers.remove(&node).is_none() {
            return;
        }
        let outs = self.state.apply(Input::PeerDisconnected { node });
        self.dispatch(outs).await;
    }

    async fn apply_and_dispatch(&mut self, input: Input) {
        let outs = self.state.apply(input);
        self.dispatch(outs).await;
    }

    async fn dispatch(&mut self, outs: Vec<Output>) {
        for out in outs {
            self.dispatch_one(out).await;
        }
        // Publish a fresh snapshot for the UI after every dispatch. Mutations
        // only happen on this task, so it's safe to send-replace without
        // racing anyone.
        let snap = self.state.snapshot();
        self.snapshot_tx.send_replace(snap);
    }

    async fn dispatch_one(&mut self, out: Output) {
        match out {
            Output::Broadcast(frame) => self.broadcast(self.fill_peer_list_addrs(frame)).await,
            Output::SendTo { to, frame } => self.send_to(to, self.fill_peer_list_addrs(frame)).await,
            Output::Mpv(cmd) => self.send_mpv(cmd).await,
            Output::Notify(notice) => log_notice(&notice),
        }
    }

    /// If `frame` is a `PresenceEvent::PeerList` emitted by the core state
    /// machine (with empty `addr_bytes` placeholders), replace each peer's
    /// third tuple element with the authoritative postcard-encoded
    /// `EndpointAddr` from our registry. Peers whose address we don't know
    /// are left with empty bytes; the receiver skips them for dialing.
    fn fill_peer_list_addrs(&self, frame: Frame) -> Frame {
        let Frame::Presence(PresenceEvent::PeerList { peers }) = frame else {
            return frame;
        };
        let rewritten = peers
            .into_iter()
            .map(|(node, nick, existing)| {
                if !existing.is_empty() {
                    return (node, nick, existing);
                }
                let bytes = self
                    .addrs
                    .get(&node)
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                (node, nick, bytes)
            })
            .collect();
        Frame::Presence(PresenceEvent::PeerList { peers: rewritten })
    }

    async fn broadcast(&self, frame: Frame) {
        if matches!(frame, Frame::Heartbeat(_)) {
            for peer in self.peers.values() {
                if let Err(e) = peer.link.send_datagram(&frame) {
                    debug!(peer = ?peer.link.remote(), error = %e, "heartbeat drop");
                }
            }
            return;
        }
        for peer in self.peers.values() {
            if let Err(e) = peer.writer.send(frame.clone()).await {
                warn!(peer = ?peer.link.remote(), error = %e, "writer queue closed");
            }
        }
    }

    async fn send_to(&self, to: NodeId, frame: Frame) {
        let Some(peer) = self.peers.get(&to) else {
            debug!(peer = ?to, "SendTo for unknown peer; dropping");
            return;
        };
        if let Err(e) = peer.writer.send(frame).await {
            warn!(peer = ?to, error = %e, "writer queue closed");
        }
    }

    async fn send_mpv(&mut self, cmd: CoreMpvCommand) {
        // Arm the echo guard *before* sending so the inbound property-change
        // edge always races the guard, not the send. We arm even when mpv is
        // disabled: the guard then harmlessly expires without matching.
        let now = now_ms();
        match cmd {
            CoreMpvCommand::Pause(p) => self.echo.record_pause(p, now),
            CoreMpvCommand::Seek { media_pos_ms } => self.echo.record_seek(media_pos_ms, now),
            CoreMpvCommand::SetSpeed { speed_centi } => self.echo.record_speed(speed_centi, now),
        }

        let Some(mpv) = self.mpv.as_ref() else {
            debug!("mpv disabled; would have sent {cmd:?}");
            return;
        };
        if let Err(e) = mpv.send_core(cmd).await {
            warn!(error = %e, "mpv command failed");
        }
    }

    async fn shutdown(mut self) {
        info!("shutting down");
        self.mesh.close().await;
        if let Some(mpv) = self.mpv.take() {
            let exit = mpv.shutdown().await;
            debug!(?exit, "mpv stopped");
        }
    }
}

/// Receive from an `Option<Receiver>`, treating `None` as "park forever".
/// Makes the `tokio::select!` branch that polls `ui_events` a no-op when no
/// UI is attached, without needing a `if let Some(rx)` branch in the macro.
async fn recv_opt<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

fn log_notice(n: &Notice) {
    match n {
        Notice::MediaMismatch {
            peer,
            severity,
            peer_media,
            our_media,
        } => {
            warn!(
                peer = ?peer,
                ?severity,
                ?peer_media,
                ?our_media,
                "media mismatch"
            );
        }
        Notice::DuplicateDropped { peer, seq } => {
            debug!(peer = ?peer, seq, "dropped duplicate control event");
        }
        Notice::ConflictDiscarded { from, seq } => {
            info!(peer = ?from, seq, "discarded conflicting control event");
        }
        Notice::StaleReferenceSkipped { peer } => {
            debug!(peer = ?peer, "skipped stale drift reference");
        }
    }
}
