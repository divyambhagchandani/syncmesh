//! Room state machine.
//!
//! This is the central `apply(Input) -> Vec<Output>` pure function that turns
//! inbound events (peer frames, mpv updates, user actions, wall-clock ticks)
//! into outbound effects (broadcasts, mpv commands, UI notices). Every task in
//! the bin crate funnels through here; keeping the function pure lets us
//! deterministically test 3-peer scenarios with simulated time.

use std::collections::{BTreeMap, VecDeque};

use crate::conflict::{ConflictOutcome, resolve};
use crate::drift::{DriftAction, LocalPlayback, ReferencePeer, check_drift};
use crate::media::{MatchResult, match_media};
use crate::node_id::NodeId;
use crate::protocol::{
    ChatMessage, ControlAction, ControlEvent, Frame, MediaId, PresenceEvent, StateHeartbeat,
};
use crate::ready::{ReadyGate, ReadyState};
use crate::rtt::RttEstimator;
use crate::snapshot::{LocalSnapshot, PeerSnapshot, RoomSnapshot};

pub const SPEED_NORMAL_CENTI: u16 = 100;
pub const SPEED_DOWN_CENTI: u16 = 95;
pub const SPEED_UP_CENTI: u16 = 105;

/// Ring-buffer capacity for chat messages (plan decision 22).
const CHAT_RING_CAPACITY: usize = 200;

/// How long a peer heartbeat remains usable as a drift reference before we
/// consider the peer stale and skip drift correction against it.
const STALE_HEARTBEAT_MS: u64 = 5_000;

/// Our own playback snapshot, updated when mpv reports a state change or when
/// we apply a remote control event locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackState {
    pub media_pos_ms: u64,
    pub paused: bool,
    pub speed_centi: u16,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            media_pos_ms: 0,
            paused: true,
            speed_centi: SPEED_NORMAL_CENTI,
        }
    }
}

/// Per-peer tracking.
#[derive(Debug, Clone)]
pub struct PeerState {
    pub nickname: String,
    pub rtt: RttEstimator,
    pub last_heartbeat: Option<StateHeartbeat>,
    pub last_heartbeat_received_at_ms: u64,
    /// Last `seq` we've already applied from this peer; used for dedup.
    pub last_applied_seq: Option<u64>,
}

impl PeerState {
    fn new(nickname: String) -> Self {
        Self {
            nickname,
            rtt: RttEstimator::new(),
            last_heartbeat: None,
            last_heartbeat_received_at_ms: 0,
            last_applied_seq: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Input {
    /// Periodic 1 Hz tick (wall-clock). Drives drift correction and heartbeat
    /// emission.
    Tick {
        now_ms: u64,
    },
    /// A wire frame arrived from a peer. If iroh has produced a fresh RTT
    /// sample since the last frame, pass it here to feed the EWMA.
    FrameReceived {
        from: NodeId,
        frame: Frame,
        received_at_ms: u64,
        rtt_sample_ms: Option<u32>,
    },
    /// mpv reported a new playback state (new position, pause toggled, etc.).
    /// The net layer decides whether the change is user-initiated or echoed
    /// from an inbound control event — only user-initiated changes should
    /// reach this input.
    LocalControl {
        action: ControlAction,
        now_ms: u64,
    },
    /// User flipped the local ready flag.
    LocalReady {
        ready: bool,
        now_ms: u64,
    },
    /// User typed a chat message.
    LocalChat {
        text: String,
        now_ms: u64,
    },
    /// mpv's position/pause/speed updated passively (not user-initiated).
    /// Keeps the local `PlaybackState` fresh so heartbeats and drift are
    /// accurate.
    MpvStateUpdate {
        media_pos_ms: u64,
        paused: bool,
        speed_centi: u16,
    },
    /// Local mpv loaded a new file.
    LocalMediaChanged {
        media: MediaId,
        now_ms: u64,
    },
    /// Peer connected. The net layer gives us its announced nickname.
    PeerConnected {
        node: NodeId,
        nickname: String,
    },
    PeerDisconnected {
        node: NodeId,
    },
    /// User toggled the "override" escape hatch on the ready gate.
    SetOverride {
        enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Send to every mesh peer.
    Broadcast(Frame),
    /// Send to exactly one peer (e.g. `PeerList` to a new joiner).
    SendTo { to: NodeId, frame: Frame },
    /// Command to the local mpv.
    Mpv(MpvCommand),
    /// UI-level notice.
    Notify(Notice),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpvCommand {
    Pause(bool),
    Seek { media_pos_ms: u64 },
    SetSpeed { speed_centi: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    MediaMismatch {
        peer: NodeId,
        severity: MatchResult,
        peer_media: MediaId,
        our_media: Option<MediaId>,
    },
    DuplicateDropped {
        peer: NodeId,
        seq: u64,
    },
    ConflictDiscarded {
        from: NodeId,
        seq: u64,
    },
    StaleReferenceSkipped {
        peer: NodeId,
    },
}

/// Authoritative room state for this peer.
#[derive(Debug)]
pub struct RoomState {
    local: NodeId,
    local_seq: u64,
    local_nickname: String,
    local_playback: PlaybackState,
    local_media: Option<MediaId>,

    peers: BTreeMap<NodeId, PeerState>,

    ready_gate: ReadyGate,
    local_ready: bool,

    last_applied_control: Option<ControlEvent>,

    chat_ring: VecDeque<ChatMessage>,
    chat_capacity: usize,
}

impl RoomState {
    pub fn new(local: NodeId, local_nickname: String) -> Self {
        let mut ready_gate = ReadyGate::new();
        ready_gate.set(local, false);
        Self {
            local,
            local_seq: 0,
            local_nickname,
            local_playback: PlaybackState::default(),
            local_media: None,
            peers: BTreeMap::new(),
            ready_gate,
            local_ready: false,
            last_applied_control: None,
            chat_ring: VecDeque::with_capacity(CHAT_RING_CAPACITY),
            chat_capacity: CHAT_RING_CAPACITY,
        }
    }

    pub fn local_node(&self) -> NodeId {
        self.local
    }

    pub fn local_playback(&self) -> PlaybackState {
        self.local_playback
    }

    pub fn local_nickname(&self) -> &str {
        &self.local_nickname
    }

    pub fn local_ready(&self) -> bool {
        self.local_ready
    }

    pub fn local_media(&self) -> Option<&MediaId> {
        self.local_media.as_ref()
    }

    pub fn peers(&self) -> impl Iterator<Item = (&NodeId, &PeerState)> {
        self.peers.iter()
    }

    pub fn peer(&self, node: &NodeId) -> Option<&PeerState> {
        self.peers.get(node)
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn chat(&self) -> impl Iterator<Item = &ChatMessage> {
        self.chat_ring.iter()
    }

    pub fn ready_state(&self) -> ReadyState {
        self.ready_gate.state()
    }

    pub fn ready_gate(&self) -> &ReadyGate {
        &self.ready_gate
    }

    /// Project the authoritative state into an immutable [`RoomSnapshot`]
    /// suitable for UI rendering. Cheap clone of every field we expose —
    /// chat ring included, so the UI can render scrollback without reaching
    /// back into the live state.
    pub fn snapshot(&self) -> RoomSnapshot {
        let local = LocalSnapshot {
            node: self.local,
            nickname: self.local_nickname.clone(),
            ready: self.local_ready,
            playback: self.local_playback,
            media: self.local_media.clone(),
        };

        let peers = self
            .peers
            .iter()
            .map(|(node, peer)| {
                let ready = self.ready_gate.get(node).unwrap_or(false);
                let rtt_ms = peer.rtt.estimate_ms();
                let drift_ms = peer.last_heartbeat.as_ref().map(|hb| {
                    #[allow(clippy::cast_possible_wrap)]
                    let local = self.local_playback.media_pos_ms as i64;
                    #[allow(clippy::cast_possible_wrap)]
                    let remote = hb.media_pos_ms as i64;
                    local - remote
                });
                let media_match = peer.last_heartbeat.as_ref().and_then(|hb| {
                    let our_media = self.local_media.as_ref()?;
                    let their_media = hb.media_id.as_ref()?;
                    Some(match_media(our_media, their_media))
                });
                PeerSnapshot {
                    node: *node,
                    nickname: peer.nickname.clone(),
                    ready,
                    rtt_ms,
                    drift_ms,
                    media_match,
                }
            })
            .collect();

        RoomSnapshot {
            local,
            peers,
            chat: self.chat_ring.iter().cloned().collect(),
            ready_state: self.ready_gate.state(),
            override_enabled: self.ready_gate.is_override_enabled(),
        }
    }

    /// Drive the state machine with one input. Pure: no I/O, no side effects.
    pub fn apply(&mut self, input: Input) -> Vec<Output> {
        match input {
            Input::Tick { now_ms } => self.on_tick(now_ms),
            Input::FrameReceived {
                from,
                frame,
                received_at_ms,
                rtt_sample_ms,
            } => self.on_frame(from, frame, received_at_ms, rtt_sample_ms),
            Input::LocalControl { action, now_ms } => self.on_local_control(action, now_ms),
            Input::LocalReady { ready, now_ms } => self.on_local_ready(ready, now_ms),
            Input::LocalChat { text, now_ms } => self.on_local_chat(text, now_ms),
            Input::MpvStateUpdate {
                media_pos_ms,
                paused,
                speed_centi,
            } => {
                self.local_playback = PlaybackState {
                    media_pos_ms,
                    paused,
                    speed_centi,
                };
                Vec::new()
            }
            Input::LocalMediaChanged { media, now_ms } => self.on_local_media(media, now_ms),
            Input::PeerConnected { node, nickname } => self.on_peer_connected(node, nickname),
            Input::PeerDisconnected { node } => self.on_peer_disconnected(node),
            Input::SetOverride { enabled } => {
                self.ready_gate.set_override(enabled);
                Vec::new()
            }
        }
    }

    // --- Tick ---

    fn on_tick(&mut self, now_ms: u64) -> Vec<Output> {
        let mut outs = Vec::new();

        // 1. Emit our own heartbeat.
        let hb = StateHeartbeat {
            origin: self.local,
            origin_ts_ms: now_ms,
            media_pos_ms: self.local_playback.media_pos_ms,
            paused: self.local_playback.paused,
            speed_centi: self.local_playback.speed_centi,
            ready: self.local_ready,
            media_id: self.local_media.clone(),
        };
        outs.push(Output::Broadcast(Frame::Heartbeat(hb)));

        // 2. Drift correction against the freshest non-stale reference peer.
        if let Some((ref_node, reference)) = self.pick_reference(now_ms) {
            let local = LocalPlayback {
                media_pos_ms: self.local_playback.media_pos_ms,
                paused: self.local_playback.paused,
            };
            match check_drift(&reference, &local, now_ms) {
                DriftAction::None => {}
                DriftAction::Slowdown => outs.push(Output::Mpv(MpvCommand::SetSpeed {
                    speed_centi: SPEED_DOWN_CENTI,
                })),
                DriftAction::Speedup => outs.push(Output::Mpv(MpvCommand::SetSpeed {
                    speed_centi: SPEED_UP_CENTI,
                })),
                DriftAction::Seek { target_ms } => {
                    outs.push(Output::Mpv(MpvCommand::Seek {
                        media_pos_ms: target_ms,
                    }));
                }
            }
            // If we're in the deadband and currently at a non-normal speed,
            // restore normal speed so we don't overshoot.
            if self.local_playback.speed_centi != SPEED_NORMAL_CENTI {
                if let DriftAction::None = check_drift(&reference, &local, now_ms) {
                    outs.push(Output::Mpv(MpvCommand::SetSpeed {
                        speed_centi: SPEED_NORMAL_CENTI,
                    }));
                }
            }
            // Emit a stale-reference notice only when we actually *had* a
            // candidate but skipped it; the `pick_reference` return already
            // filters staleness, so nothing to do here. The Notice variant is
            // wired up below for the other branch.
            let _ = ref_node;
        } else if let Some(stale) = self.stalest_peer_with_heartbeat(now_ms) {
            outs.push(Output::Notify(Notice::StaleReferenceSkipped {
                peer: stale,
            }));
        }

        outs
    }

    fn pick_reference(&self, now_ms: u64) -> Option<(NodeId, ReferencePeer)> {
        let mut best: Option<(NodeId, &StateHeartbeat, u32)> = None;
        for (node, peer) in &self.peers {
            let Some(hb) = &peer.last_heartbeat else {
                continue;
            };
            if now_ms.saturating_sub(peer.last_heartbeat_received_at_ms) > STALE_HEARTBEAT_MS {
                continue;
            }
            let one_way = peer.rtt.one_way_ms();
            match best {
                None => best = Some((*node, hb, one_way)),
                Some((_, best_hb, _)) if hb.origin_ts_ms > best_hb.origin_ts_ms => {
                    best = Some((*node, hb, one_way));
                }
                _ => {}
            }
        }
        best.map(|(node, hb, one_way)| {
            (
                node,
                ReferencePeer {
                    media_pos_ms: hb.media_pos_ms,
                    origin_ts_ms: hb.origin_ts_ms,
                    paused: hb.paused,
                    one_way_ms: one_way,
                },
            )
        })
    }

    fn stalest_peer_with_heartbeat(&self, now_ms: u64) -> Option<NodeId> {
        self.peers
            .iter()
            .filter_map(|(n, p)| {
                p.last_heartbeat
                    .as_ref()
                    .map(|_| (*n, now_ms.saturating_sub(p.last_heartbeat_received_at_ms)))
            })
            .filter(|(_, age)| *age > STALE_HEARTBEAT_MS)
            .max_by_key(|(_, age)| *age)
            .map(|(n, _)| n)
    }

    // --- Frame ingestion ---

    fn on_frame(
        &mut self,
        from: NodeId,
        frame: Frame,
        received_at_ms: u64,
        rtt_sample_ms: Option<u32>,
    ) -> Vec<Output> {
        if let Some(rtt) = rtt_sample_ms {
            if let Some(peer) = self.peers.get_mut(&from) {
                peer.rtt.sample(rtt);
            }
        }
        match frame {
            Frame::Control(ev) => self.on_inbound_control(from, ev, received_at_ms),
            Frame::Heartbeat(hb) => self.on_inbound_heartbeat(from, &hb, received_at_ms),
            Frame::Presence(p) => self.on_inbound_presence(from, p),
            Frame::Chat(msg) => self.on_inbound_chat(msg),
        }
    }

    fn on_inbound_control(
        &mut self,
        from: NodeId,
        ev: ControlEvent,
        _received_at_ms: u64,
    ) -> Vec<Output> {
        // Dedup by (origin, seq).
        if let Some(peer) = self.peers.get_mut(&from) {
            if let Some(last) = peer.last_applied_seq {
                if ev.seq <= last {
                    return vec![Output::Notify(Notice::DuplicateDropped {
                        peer: from,
                        seq: ev.seq,
                    })];
                }
            }
        }

        // Conflict resolution against last applied control event.
        if let Some(prior) = &self.last_applied_control {
            if let ConflictOutcome::Discard = resolve(prior, &ev) {
                if let Some(peer) = self.peers.get_mut(&from) {
                    peer.last_applied_seq = Some(ev.seq);
                }
                return vec![Output::Notify(Notice::ConflictDiscarded {
                    from,
                    seq: ev.seq,
                })];
            }
        }

        let mut outs = Vec::new();
        match &ev.action {
            ControlAction::Pause { media_pos_ms } => {
                self.local_playback.paused = true;
                self.local_playback.media_pos_ms = *media_pos_ms;
                outs.push(Output::Mpv(MpvCommand::Seek {
                    media_pos_ms: *media_pos_ms,
                }));
                outs.push(Output::Mpv(MpvCommand::Pause(true)));
            }
            ControlAction::Play { media_pos_ms } => {
                self.local_playback.paused = false;
                self.local_playback.media_pos_ms = *media_pos_ms;
                outs.push(Output::Mpv(MpvCommand::Seek {
                    media_pos_ms: *media_pos_ms,
                }));
                outs.push(Output::Mpv(MpvCommand::Pause(false)));
            }
            ControlAction::Seek { media_pos_ms } => {
                self.local_playback.media_pos_ms = *media_pos_ms;
                outs.push(Output::Mpv(MpvCommand::Seek {
                    media_pos_ms: *media_pos_ms,
                }));
            }
            ControlAction::SetSpeed { speed_centi } => {
                self.local_playback.speed_centi = *speed_centi;
                outs.push(Output::Mpv(MpvCommand::SetSpeed {
                    speed_centi: *speed_centi,
                }));
            }
            ControlAction::MediaChanged { id } => {
                if let Some(ours) = &self.local_media {
                    let m = match_media(ours, id);
                    if m != MatchResult::Identical {
                        outs.push(Output::Notify(Notice::MediaMismatch {
                            peer: from,
                            severity: m,
                            peer_media: id.clone(),
                            our_media: Some(ours.clone()),
                        }));
                    }
                } else {
                    outs.push(Output::Notify(Notice::MediaMismatch {
                        peer: from,
                        severity: MatchResult::Different,
                        peer_media: id.clone(),
                        our_media: None,
                    }));
                }
            }
        }

        if let Some(peer) = self.peers.get_mut(&from) {
            peer.last_applied_seq = Some(ev.seq);
        }
        self.last_applied_control = Some(ev);

        outs
    }

    fn on_inbound_heartbeat(
        &mut self,
        from: NodeId,
        hb: &StateHeartbeat,
        received_at_ms: u64,
    ) -> Vec<Output> {
        let mut outs = Vec::new();
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.last_heartbeat = Some(hb.clone());
            peer.last_heartbeat_received_at_ms = received_at_ms;
        }
        // Update ready gate from heartbeat (authoritative signal).
        self.ready_gate.set(from, hb.ready);

        // Media mismatch check.
        if let Some(peer_media) = &hb.media_id {
            match &self.local_media {
                Some(local_media) => {
                    let m = match_media(local_media, peer_media);
                    if m != MatchResult::Identical {
                        outs.push(Output::Notify(Notice::MediaMismatch {
                            peer: from,
                            severity: m,
                            peer_media: peer_media.clone(),
                            our_media: Some(local_media.clone()),
                        }));
                    }
                }
                None => outs.push(Output::Notify(Notice::MediaMismatch {
                    peer: from,
                    severity: MatchResult::Different,
                    peer_media: peer_media.clone(),
                    our_media: None,
                })),
            }
        }
        outs
    }

    fn on_inbound_presence(&mut self, from: NodeId, p: PresenceEvent) -> Vec<Output> {
        match p {
            PresenceEvent::Join { node, nickname } => {
                self.peers
                    .entry(node)
                    .or_insert_with(|| PeerState::new(nickname.clone()));
                if let Some(peer) = self.peers.get_mut(&node) {
                    peer.nickname = nickname;
                }
                self.ready_gate.set(node, false);
                Vec::new()
            }
            PresenceEvent::Leave { node } => {
                self.peers.remove(&node);
                self.ready_gate.remove(&node);
                Vec::new()
            }
            PresenceEvent::Ready { node, ready } => {
                self.ready_gate.set(node, ready);
                Vec::new()
            }
            PresenceEvent::Rename { node, nickname } => {
                if let Some(peer) = self.peers.get_mut(&node) {
                    peer.nickname = nickname;
                }
                Vec::new()
            }
            PresenceEvent::PeerList { peers } => {
                // A peer gave us their roster. Record any unknown nodes as
                // peers; the net layer will dial them using the opaque
                // `addr_bytes` third field (which this crate does not inspect).
                let _ = from;
                for (node, nickname, _addr_bytes) in peers {
                    if node == self.local {
                        continue;
                    }
                    self.peers
                        .entry(node)
                        .or_insert_with(|| PeerState::new(nickname.clone()));
                    if let Some(peer) = self.peers.get_mut(&node) {
                        peer.nickname = nickname;
                    }
                    self.ready_gate.set(node, false);
                }
                Vec::new()
            }
            PresenceEvent::AddrAnnounce { .. } => {
                // Address bookkeeping is entirely a transport-layer concern;
                // the core state machine ignores `AddrAnnounce`. The bin
                // layer snoops these before calling `apply` and updates its
                // address registry there.
                let _ = from;
                Vec::new()
            }
        }
    }

    fn on_inbound_chat(&mut self, msg: ChatMessage) -> Vec<Output> {
        self.push_chat(msg);
        Vec::new()
    }

    // --- Local actions ---

    fn on_local_control(&mut self, action: ControlAction, now_ms: u64) -> Vec<Output> {
        // Update local state to match the action.
        match &action {
            ControlAction::Pause { media_pos_ms } => {
                self.local_playback.paused = true;
                self.local_playback.media_pos_ms = *media_pos_ms;
            }
            ControlAction::Play { media_pos_ms } => {
                self.local_playback.paused = false;
                self.local_playback.media_pos_ms = *media_pos_ms;
            }
            ControlAction::Seek { media_pos_ms } => {
                self.local_playback.media_pos_ms = *media_pos_ms;
            }
            ControlAction::SetSpeed { speed_centi } => {
                self.local_playback.speed_centi = *speed_centi;
            }
            ControlAction::MediaChanged { id } => {
                self.local_media = Some(id.clone());
            }
        }

        self.local_seq += 1;
        let ev = ControlEvent {
            origin: self.local,
            origin_ts_ms: now_ms,
            seq: self.local_seq,
            action,
        };
        self.last_applied_control = Some(ev.clone());
        vec![Output::Broadcast(Frame::Control(ev))]
    }

    fn on_local_ready(&mut self, ready: bool, _now_ms: u64) -> Vec<Output> {
        self.local_ready = ready;
        self.ready_gate.set(self.local, ready);
        vec![Output::Broadcast(Frame::Presence(PresenceEvent::Ready {
            node: self.local,
            ready,
        }))]
    }

    fn on_local_chat(&mut self, text: String, now_ms: u64) -> Vec<Output> {
        let msg = ChatMessage {
            origin: self.local,
            origin_ts_ms: now_ms,
            text,
        };
        self.push_chat(msg.clone());
        vec![Output::Broadcast(Frame::Chat(msg))]
    }

    fn on_local_media(&mut self, media: MediaId, now_ms: u64) -> Vec<Output> {
        self.local_media = Some(media.clone());
        self.local_seq += 1;
        let ev = ControlEvent {
            origin: self.local,
            origin_ts_ms: now_ms,
            seq: self.local_seq,
            action: ControlAction::MediaChanged { id: media },
        };
        self.last_applied_control = Some(ev.clone());
        vec![Output::Broadcast(Frame::Control(ev))]
    }

    fn on_peer_connected(&mut self, node: NodeId, nickname: String) -> Vec<Output> {
        if node == self.local {
            return Vec::new();
        }
        let mut new_peer = false;
        self.peers
            .entry(node)
            .and_modify(|p| p.nickname.clone_from(&nickname))
            .or_insert_with(|| {
                new_peer = true;
                PeerState::new(nickname.clone())
            });
        self.ready_gate.set(node, false);

        if !new_peer {
            return Vec::new();
        }
        // Send the new peer the current roster so they can dial everyone.
        // `addr_bytes` is left empty here; the bin layer intercepts
        // `Output::SendTo` with a `PeerList` and rewrites the third tuple
        // field from its address registry. This keeps the core crate free of
        // any transport-layer address type.
        let peers_list: Vec<(NodeId, String, Vec<u8>)> =
            std::iter::once((self.local, self.local_nickname.clone(), Vec::new()))
                .chain(
                    self.peers
                        .iter()
                        .filter(|(n, _)| **n != node)
                        .map(|(n, p)| (*n, p.nickname.clone(), Vec::new())),
                )
                .collect();
        let mut outs = vec![
            Output::SendTo {
                to: node,
                frame: Frame::Presence(PresenceEvent::PeerList { peers: peers_list }),
            },
            Output::Broadcast(Frame::Presence(PresenceEvent::Join { node, nickname })),
        ];
        // Auto-seek the new joiner to our current position (plan decision 12).
        // In a real mesh, each existing peer would send this — for v1 we send
        // it unconditionally; the joiner deduplicates by seq.
        self.local_seq += 1;
        let seek_ev = ControlEvent {
            origin: self.local,
            origin_ts_ms: 0, // net layer should overwrite with its own clock
            seq: self.local_seq,
            action: ControlAction::Seek {
                media_pos_ms: self.local_playback.media_pos_ms,
            },
        };
        outs.push(Output::SendTo {
            to: node,
            frame: Frame::Control(seek_ev),
        });
        outs
    }

    fn on_peer_disconnected(&mut self, node: NodeId) -> Vec<Output> {
        self.peers.remove(&node);
        self.ready_gate.remove(&node);
        vec![Output::Broadcast(Frame::Presence(PresenceEvent::Leave {
            node,
        }))]
    }

    fn push_chat(&mut self, msg: ChatMessage) {
        if self.chat_ring.len() >= self.chat_capacity {
            self.chat_ring.pop_front();
        }
        self.chat_ring.push_back(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ChatMessage, ControlAction, PresenceEvent};

    fn n(b: u8) -> NodeId {
        NodeId::from_bytes([b; 32])
    }

    fn new_room() -> RoomState {
        RoomState::new(n(1), "me".into())
    }

    fn hb(origin: NodeId, ts: u64, pos: u64, paused: bool, ready: bool) -> StateHeartbeat {
        StateHeartbeat {
            origin,
            origin_ts_ms: ts,
            media_pos_ms: pos,
            paused,
            speed_centi: SPEED_NORMAL_CENTI,
            ready,
            media_id: None,
        }
    }

    // --- Construction ---

    #[test]
    fn new_room_has_self_in_ready_gate() {
        let r = new_room();
        assert_eq!(r.local_node(), n(1));
        assert_eq!(r.peer_count(), 0);
        // Self is not ready by default.
        assert_eq!(r.ready_state(), ReadyState::Pending);
    }

    // --- Peer join/leave ---

    #[test]
    fn peer_connect_emits_peerlist_and_join_broadcast() {
        let mut r = new_room();
        let outs = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "alice".into(),
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::SendTo {
                to,
                frame: Frame::Presence(PresenceEvent::PeerList { .. })
            } if *to == n(2)
        )));
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Broadcast(Frame::Presence(PresenceEvent::Join { node, .. })) if *node == n(2)
        )));
        assert_eq!(r.peer_count(), 1);
    }

    #[test]
    fn second_peer_connect_does_not_duplicate_peerlist() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "alice".into(),
        });
        // Reconnect with different nickname: updates in place, no roster resend.
        let outs = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "alice2".into(),
        });
        assert!(outs.iter().all(|o| !matches!(
            o,
            Output::SendTo {
                frame: Frame::Presence(PresenceEvent::PeerList { .. }),
                ..
            }
        )));
        assert_eq!(r.peer(&n(2)).unwrap().nickname, "alice2");
    }

    #[test]
    fn peer_disconnect_removes_and_broadcasts_leave() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "alice".into(),
        });
        let outs = r.apply(Input::PeerDisconnected { node: n(2) });
        assert_eq!(r.peer_count(), 0);
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Broadcast(Frame::Presence(PresenceEvent::Leave { node })) if *node == n(2)))
        );
    }

    // --- Ready gate ---

    #[test]
    fn local_ready_toggle_broadcasts_and_updates_gate() {
        let mut r = new_room();
        let outs = r.apply(Input::LocalReady {
            ready: true,
            now_ms: 100,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Broadcast(Frame::Presence(PresenceEvent::Ready { ready: true, .. }))
        )));
        assert!(r.local_ready());
        // Self-only gate: single ready peer → AllReady.
        assert_eq!(r.ready_state(), ReadyState::AllReady);
    }

    #[test]
    fn remote_ready_via_presence_updates_gate() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let _ = r.apply(Input::LocalReady {
            ready: true,
            now_ms: 1,
        });
        assert_eq!(r.ready_state(), ReadyState::Pending);

        let _ = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Presence(PresenceEvent::Ready {
                node: n(2),
                ready: true,
            }),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert_eq!(r.ready_state(), ReadyState::AllReady);
    }

    // --- Control event dedup and conflict ---

    #[test]
    fn duplicate_seq_is_dropped_with_notice() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 1000,
            seq: 5,
            action: ControlAction::Pause { media_pos_ms: 100 },
        };
        let outs1 = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev.clone()),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(
            outs1
                .iter()
                .any(|o| matches!(o, Output::Mpv(MpvCommand::Pause(true))))
        );
        // Re-send same seq → dropped.
        let outs2 = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 20,
            rtt_sample_ms: None,
        });
        assert!(
            outs2
                .iter()
                .any(|o| matches!(o, Output::Notify(Notice::DuplicateDropped { seq: 5, .. })))
        );
        assert!(!outs2.iter().any(|o| matches!(o, Output::Mpv(_))));
    }

    #[test]
    fn conflicting_control_is_discarded_by_lex_rule() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let _ = r.apply(Input::PeerConnected {
            node: n(3),
            nickname: "b".into(),
        });
        // Apply pause from n(2) at ts=1000.
        let first = ControlEvent {
            origin: n(2),
            origin_ts_ms: 1000,
            seq: 1,
            action: ControlAction::Pause { media_pos_ms: 100 },
        };
        let _ = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(first),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        // n(3) sends a pause at ts=1100 → within window, larger tuple → discard.
        let second = ControlEvent {
            origin: n(3),
            origin_ts_ms: 1100,
            seq: 1,
            action: ControlAction::Pause { media_pos_ms: 200 },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(3),
            frame: Frame::Control(second),
            received_at_ms: 20,
            rtt_sample_ms: None,
        });
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Notify(Notice::ConflictDiscarded { from, seq: 1 }) if *from == n(3)))
        );
        assert!(!outs.iter().any(|o| matches!(o, Output::Mpv(_))));
    }

    // --- Inbound control applies to mpv + updates local state ---

    #[test]
    fn inbound_pause_applies_to_mpv_and_updates_local_state() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 1000,
            seq: 1,
            action: ControlAction::Pause { media_pos_ms: 5000 },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 10,
            rtt_sample_ms: Some(30),
        });
        let seek = outs.iter().find_map(|o| match o {
            Output::Mpv(MpvCommand::Seek { media_pos_ms }) => Some(*media_pos_ms),
            _ => None,
        });
        let pause = outs
            .iter()
            .any(|o| matches!(o, Output::Mpv(MpvCommand::Pause(true))));
        assert_eq!(seek, Some(5000));
        assert!(pause);
        assert!(r.local_playback().paused);
        assert_eq!(r.local_playback().media_pos_ms, 5000);
        // RTT sample recorded.
        assert_eq!(r.peer(&n(2)).unwrap().rtt.estimate_ms(), Some(30));
    }

    #[test]
    fn inbound_set_speed_updates_local_and_mpv() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 1000,
            seq: 1,
            action: ControlAction::SetSpeed { speed_centi: 95 },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Mpv(MpvCommand::SetSpeed { speed_centi: 95 })))
        );
        assert_eq!(r.local_playback().speed_centi, 95);
    }

    // --- Heartbeat & drift ---

    #[test]
    fn tick_emits_heartbeat() {
        let mut r = new_room();
        let outs = r.apply(Input::Tick { now_ms: 1_000 });
        let found = outs
            .iter()
            .any(|o| matches!(o, Output::Broadcast(Frame::Heartbeat(_))));
        assert!(found, "expected heartbeat broadcast, got {outs:?}");
    }

    #[test]
    fn tick_with_drifted_peer_emits_speedup() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        // Local is at 10_000; peer heartbeat says they're at 10_500 at the
        // same wall-clock → I'm 500 ms behind → Speedup.
        r.apply(Input::MpvStateUpdate {
            media_pos_ms: 10_000,
            paused: false,
            speed_centi: SPEED_NORMAL_CENTI,
        });
        let _ = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Heartbeat(hb(n(2), 1_000, 10_500, false, true)),
            received_at_ms: 1_000,
            rtt_sample_ms: Some(0),
        });
        let outs = r.apply(Input::Tick { now_ms: 1_000 });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Mpv(MpvCommand::SetSpeed {
                speed_centi: SPEED_UP_CENTI
            })
        )));
    }

    #[test]
    fn tick_with_hard_drift_emits_seek() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        r.apply(Input::MpvStateUpdate {
            media_pos_ms: 10_000,
            paused: false,
            speed_centi: SPEED_NORMAL_CENTI,
        });
        let _ = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Heartbeat(hb(n(2), 1_000, 20_000, false, true)),
            received_at_ms: 1_000,
            rtt_sample_ms: None,
        });
        let outs = r.apply(Input::Tick { now_ms: 1_000 });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Mpv(MpvCommand::Seek {
                media_pos_ms: 20_000
            })
        )));
    }

    #[test]
    fn tick_skips_stale_reference() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        r.apply(Input::MpvStateUpdate {
            media_pos_ms: 10_000,
            paused: false,
            speed_centi: SPEED_NORMAL_CENTI,
        });
        let _ = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Heartbeat(hb(n(2), 1_000, 20_000, false, true)),
            received_at_ms: 1_000,
            rtt_sample_ms: None,
        });
        // Advance far beyond the 5s stale threshold.
        let outs = r.apply(Input::Tick { now_ms: 10_000 });
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Output::Mpv(MpvCommand::Seek { .. })))
        );
        assert!(outs.iter().any(
            |o| matches!(o, Output::Notify(Notice::StaleReferenceSkipped { peer }) if *peer == n(2))
        ));
    }

    // --- Local control emits broadcast ---

    #[test]
    fn local_control_emits_broadcast_with_increasing_seq() {
        let mut r = new_room();
        let outs1 = r.apply(Input::LocalControl {
            action: ControlAction::Pause { media_pos_ms: 100 },
            now_ms: 1_000,
        });
        let outs2 = r.apply(Input::LocalControl {
            action: ControlAction::Play { media_pos_ms: 200 },
            now_ms: 2_000,
        });
        let seq1 = outs1.iter().find_map(|o| match o {
            Output::Broadcast(Frame::Control(ev)) => Some(ev.seq),
            _ => None,
        });
        let seq2 = outs2.iter().find_map(|o| match o {
            Output::Broadcast(Frame::Control(ev)) => Some(ev.seq),
            _ => None,
        });
        assert!(seq1.is_some() && seq2.is_some());
        assert!(seq2.unwrap() > seq1.unwrap());
        assert!(!r.local_playback().paused);
        assert_eq!(r.local_playback().media_pos_ms, 200);
    }

    // --- Chat ---

    #[test]
    fn local_chat_broadcasts_and_rings() {
        let mut r = new_room();
        let _ = r.apply(Input::LocalChat {
            text: "hello".into(),
            now_ms: 1_000,
        });
        assert_eq!(r.chat().count(), 1);
        assert_eq!(r.chat().next().unwrap().text, "hello");
    }

    #[test]
    fn chat_ring_evicts_oldest_beyond_capacity() {
        let mut r = new_room();
        for i in 0..(CHAT_RING_CAPACITY + 10) {
            r.apply(Input::LocalChat {
                text: format!("msg{i}"),
                now_ms: i as u64,
            });
        }
        assert_eq!(r.chat().count(), CHAT_RING_CAPACITY);
        // Oldest preserved should be msg10.
        assert_eq!(r.chat().next().unwrap().text, "msg10");
    }

    #[test]
    fn remote_chat_is_ringed_but_not_broadcast() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let msg = ChatMessage {
            origin: n(2),
            origin_ts_ms: 1,
            text: "hi there".into(),
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Chat(msg),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.is_empty());
        assert_eq!(r.chat().count(), 1);
    }

    // --- Media mismatch ---

    #[test]
    fn heartbeat_with_different_media_emits_mismatch_notice() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        r.apply(Input::LocalMediaChanged {
            media: MediaId {
                filename_lower: "a.mkv".into(),
                size_bytes: 100,
                duration_s: 10,
            },
            now_ms: 0,
        });
        let mut h = hb(n(2), 1_000, 0, true, false);
        h.media_id = Some(MediaId {
            filename_lower: "b.mkv".into(),
            size_bytes: 200,
            duration_s: 10,
        });
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Heartbeat(h),
            received_at_ms: 1_000,
            rtt_sample_ms: None,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Notify(Notice::MediaMismatch {
                severity: MatchResult::Different,
                ..
            })
        )));
    }

    #[test]
    fn heartbeat_with_same_name_different_size_is_soft_mismatch() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        r.apply(Input::LocalMediaChanged {
            media: MediaId {
                filename_lower: "a.mkv".into(),
                size_bytes: 100,
                duration_s: 10,
            },
            now_ms: 0,
        });
        let mut h = hb(n(2), 1_000, 0, true, false);
        h.media_id = Some(MediaId {
            filename_lower: "a.mkv".into(),
            size_bytes: 999,
            duration_s: 10,
        });
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Heartbeat(h),
            received_at_ms: 1_000,
            rtt_sample_ms: None,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Notify(Notice::MediaMismatch {
                severity: MatchResult::SameNameDifferentFile,
                ..
            })
        )));
    }

    // --- Override flag ---

    #[test]
    fn override_flag_allows_advance_with_partial_ready() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let _ = r.apply(Input::LocalReady {
            ready: true,
            now_ms: 1,
        });
        assert_eq!(r.ready_state(), ReadyState::Pending);

        r.apply(Input::SetOverride { enabled: true });
        assert_eq!(r.ready_state(), ReadyState::AllReady);
    }

    // --- Inbound control: Play / Seek / MediaChanged (Pause + SetSpeed already covered) ---

    #[test]
    fn inbound_play_seeks_and_unpauses_and_updates_local_state() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        // Start paused so we can observe the transition to playing.
        r.apply(Input::MpvStateUpdate {
            media_pos_ms: 0,
            paused: true,
            speed_centi: SPEED_NORMAL_CENTI,
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 1_000,
            seq: 1,
            action: ControlAction::Play {
                media_pos_ms: 7_500,
            },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Mpv(MpvCommand::Seek { media_pos_ms: 7_500 })))
        );
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Mpv(MpvCommand::Pause(false))))
        );
        assert!(!r.local_playback().paused);
        assert_eq!(r.local_playback().media_pos_ms, 7_500);
    }

    #[test]
    fn inbound_seek_updates_position_without_changing_pause() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        r.apply(Input::MpvStateUpdate {
            media_pos_ms: 0,
            paused: false,
            speed_centi: SPEED_NORMAL_CENTI,
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 1_000,
            seq: 1,
            action: ControlAction::Seek {
                media_pos_ms: 42_000,
            },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(
            outs.iter()
                .any(|o| matches!(o, Output::Mpv(MpvCommand::Seek { media_pos_ms: 42_000 })))
        );
        // No Pause command emitted for a bare Seek.
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Output::Mpv(MpvCommand::Pause(_))))
        );
        assert_eq!(r.local_playback().media_pos_ms, 42_000);
        assert!(!r.local_playback().paused);
    }

    #[test]
    fn inbound_media_changed_with_matching_local_emits_no_notice() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let media = MediaId {
            filename_lower: "a.mkv".into(),
            size_bytes: 100,
            duration_s: 10,
        };
        r.apply(Input::LocalMediaChanged {
            media: media.clone(),
            now_ms: 0,
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 2_000,
            seq: 1,
            action: ControlAction::MediaChanged { id: media },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, Output::Notify(Notice::MediaMismatch { .. })))
        );
    }

    #[test]
    fn inbound_media_changed_with_different_local_emits_mismatch() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        r.apply(Input::LocalMediaChanged {
            media: MediaId {
                filename_lower: "a.mkv".into(),
                size_bytes: 100,
                duration_s: 10,
            },
            now_ms: 0,
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 2_000,
            seq: 1,
            action: ControlAction::MediaChanged {
                id: MediaId {
                    filename_lower: "b.mkv".into(),
                    size_bytes: 200,
                    duration_s: 10,
                },
            },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Notify(Notice::MediaMismatch {
                severity: MatchResult::Different,
                our_media: Some(_),
                ..
            })
        )));
    }

    #[test]
    fn inbound_media_changed_without_local_media_emits_mismatch_with_none() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let ev = ControlEvent {
            origin: n(2),
            origin_ts_ms: 2_000,
            seq: 1,
            action: ControlAction::MediaChanged {
                id: MediaId {
                    filename_lower: "b.mkv".into(),
                    size_bytes: 200,
                    duration_s: 10,
                },
            },
        };
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Control(ev),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Notify(Notice::MediaMismatch {
                severity: MatchResult::Different,
                our_media: None,
                ..
            })
        )));
    }

    // --- Heartbeat media mismatch: local has no media ---

    #[test]
    fn heartbeat_with_peer_media_and_no_local_media_emits_mismatch() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "a".into(),
        });
        let mut h = hb(n(2), 1_000, 0, true, false);
        h.media_id = Some(MediaId {
            filename_lower: "only-they-have-it.mkv".into(),
            size_bytes: 500,
            duration_s: 60,
        });
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Heartbeat(h),
            received_at_ms: 1_000,
            rtt_sample_ms: None,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Notify(Notice::MediaMismatch {
                severity: MatchResult::Different,
                our_media: None,
                ..
            })
        )));
    }

    // --- Inbound presence: Join / Leave / Rename / PeerList ---

    #[test]
    fn inbound_presence_join_adds_peer_and_registers_in_gate() {
        let mut r = new_room();
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Presence(PresenceEvent::Join {
                node: n(3),
                nickname: "carol".into(),
            }),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.is_empty());
        assert_eq!(r.peer(&n(3)).unwrap().nickname, "carol");
        // Gate should now include the new peer as not-ready.
        let _ = r.apply(Input::LocalReady {
            ready: true,
            now_ms: 1,
        });
        assert_eq!(r.ready_state(), ReadyState::Pending);
    }

    #[test]
    fn inbound_presence_leave_removes_peer_and_clears_gate() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "alice".into(),
        });
        // With local ready + peer not ready → Pending.
        let _ = r.apply(Input::LocalReady {
            ready: true,
            now_ms: 1,
        });
        assert_eq!(r.ready_state(), ReadyState::Pending);

        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Presence(PresenceEvent::Leave { node: n(2) }),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.is_empty());
        assert_eq!(r.peer_count(), 0);
        // Gate cleared → only local (ready) remains → AllReady.
        assert_eq!(r.ready_state(), ReadyState::AllReady);
    }

    #[test]
    fn inbound_presence_rename_updates_nickname() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "alice".into(),
        });
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Presence(PresenceEvent::Rename {
                node: n(2),
                nickname: "alice-the-second".into(),
            }),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.is_empty());
        assert_eq!(r.peer(&n(2)).unwrap().nickname, "alice-the-second");
    }

    #[test]
    fn inbound_presence_rename_on_unknown_node_is_noop() {
        let mut r = new_room();
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Presence(PresenceEvent::Rename {
                node: n(9),
                nickname: "ghost".into(),
            }),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.is_empty());
        assert_eq!(r.peer_count(), 0);
    }

    #[test]
    fn inbound_peer_list_registers_all_nodes_and_skips_self() {
        let mut r = new_room();
        let outs = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Presence(PresenceEvent::PeerList {
                peers: vec![
                    (n(1), "me-echoed-back".into(), vec![]), // skipped: this is us
                    (n(2), "alice".into(), vec![]),
                    (n(3), "bob".into(), vec![]),
                ],
            }),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert!(outs.is_empty());
        assert_eq!(r.peer_count(), 2);
        assert_eq!(r.peer(&n(2)).unwrap().nickname, "alice");
        assert_eq!(r.peer(&n(3)).unwrap().nickname, "bob");
        assert!(r.peer(&n(1)).is_none());
    }

    #[test]
    fn inbound_peer_list_updates_existing_nickname() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "old-name".into(),
        });
        let _ = r.apply(Input::FrameReceived {
            from: n(3),
            frame: Frame::Presence(PresenceEvent::PeerList {
                peers: vec![(n(2), "new-name".into(), vec![])],
            }),
            received_at_ms: 10,
            rtt_sample_ms: None,
        });
        assert_eq!(r.peer(&n(2)).unwrap().nickname, "new-name");
    }

    // --- Local non-Pause control paths ---

    #[test]
    fn local_seek_broadcasts_and_updates_position() {
        let mut r = new_room();
        r.apply(Input::MpvStateUpdate {
            media_pos_ms: 1_000,
            paused: false,
            speed_centi: SPEED_NORMAL_CENTI,
        });
        let outs = r.apply(Input::LocalControl {
            action: ControlAction::Seek {
                media_pos_ms: 12_345,
            },
            now_ms: 5_000,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Broadcast(Frame::Control(ControlEvent {
                action: ControlAction::Seek {
                    media_pos_ms: 12_345
                },
                ..
            }))
        )));
        assert_eq!(r.local_playback().media_pos_ms, 12_345);
        // Pause state unchanged by Seek.
        assert!(!r.local_playback().paused);
    }

    #[test]
    fn local_set_speed_broadcasts_and_updates_speed() {
        let mut r = new_room();
        let outs = r.apply(Input::LocalControl {
            action: ControlAction::SetSpeed { speed_centi: 125 },
            now_ms: 0,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Broadcast(Frame::Control(ControlEvent {
                action: ControlAction::SetSpeed { speed_centi: 125 },
                ..
            }))
        )));
        assert_eq!(r.local_playback().speed_centi, 125);
    }

    #[test]
    fn local_media_changed_via_control_action_sets_local_media() {
        let mut r = new_room();
        let media = MediaId {
            filename_lower: "x.mkv".into(),
            size_bytes: 7,
            duration_s: 1,
        };
        let outs = r.apply(Input::LocalControl {
            action: ControlAction::MediaChanged { id: media.clone() },
            now_ms: 0,
        });
        assert!(outs.iter().any(|o| matches!(
            o,
            Output::Broadcast(Frame::Control(ControlEvent {
                action: ControlAction::MediaChanged { .. },
                ..
            }))
        )));
        assert_eq!(r.local_media(), Some(&media));
    }

    // --- Snapshot projection ---

    #[test]
    fn snapshot_reflects_local_fields() {
        let r = new_room();
        let s = r.snapshot();
        assert_eq!(s.local.node, n(1));
        assert_eq!(s.local.nickname, "me");
        assert!(!s.local.ready);
        assert_eq!(s.local.playback, PlaybackState::default());
        assert!(s.local.media.is_none());
        assert!(s.peers.is_empty());
        assert!(s.chat.is_empty());
        assert_eq!(s.ready_state, ReadyState::Pending);
        assert!(!s.override_enabled);
    }

    #[test]
    fn snapshot_includes_peers_with_nicknames_and_ready_flags() {
        let mut r = new_room();
        let _ = r.apply(Input::PeerConnected {
            node: n(2),
            nickname: "alice".into(),
        });
        let _ = r.apply(Input::FrameReceived {
            from: n(2),
            frame: Frame::Presence(PresenceEvent::Ready {
                node: n(2),
                ready: true,
            }),
            received_at_ms: 1,
            rtt_sample_ms: None,
        });
        let s = r.snapshot();
        assert_eq!(s.peers.len(), 1);
        assert_eq!(s.peers[0].node, n(2));
        assert_eq!(s.peers[0].nickname, "alice");
        assert!(s.peers[0].ready);
    }

    #[test]
    fn snapshot_chat_captures_ring_contents() {
        let mut r = new_room();
        let _ = r.apply(Input::LocalChat {
            text: "hello".into(),
            now_ms: 10,
        });
        let _ = r.apply(Input::LocalChat {
            text: "world".into(),
            now_ms: 20,
        });
        let s = r.snapshot();
        assert_eq!(s.chat.len(), 2);
        assert_eq!(s.chat[0].text, "hello");
        assert_eq!(s.chat[1].text, "world");
    }

    #[test]
    fn snapshot_override_flag_tracked() {
        let mut r = new_room();
        let _ = r.apply(Input::SetOverride { enabled: true });
        assert!(r.snapshot().override_enabled);
        let _ = r.apply(Input::SetOverride { enabled: false });
        assert!(!r.snapshot().override_enabled);
    }
}
