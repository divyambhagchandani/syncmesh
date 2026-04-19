//! Immutable projection of `RoomState` for UI rendering.
//!
//! The event loop owns the authoritative `RoomState` and mutates it serially.
//! The UI thread needs a read-only view without taking a lock on the hot
//! path. `RoomState::snapshot()` returns a `RoomSnapshot` by cloning the
//! fields the UI cares about; the bin then publishes it on a `watch`
//! channel that the UI task reads at its own cadence.
//!
//! Everything in this module is a plain struct, clone-able, with no
//! invariants to maintain — that's the whole point. If the UI needs a new
//! piece of data, add a field here, populate it in `snapshot()`, and
//! render it.

use crate::media::MatchResult;
use crate::node_id::NodeId;
use crate::protocol::{ChatMessage, MediaId};
use crate::ready::ReadyState;
use crate::state::PlaybackState;

/// Full snapshot of room state for the UI to render.
#[derive(Debug, Clone)]
pub struct RoomSnapshot {
    pub local: LocalSnapshot,
    pub peers: Vec<PeerSnapshot>,
    pub chat: Vec<ChatMessage>,
    pub ready_state: ReadyState,
    pub override_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct LocalSnapshot {
    pub node: NodeId,
    pub nickname: String,
    pub ready: bool,
    pub playback: PlaybackState,
    pub media: Option<MediaId>,
}

#[derive(Debug, Clone)]
pub struct PeerSnapshot {
    pub node: NodeId,
    pub nickname: String,
    pub ready: bool,
    /// EWMA round-trip time to this peer in ms, if iroh has sampled at least
    /// one path to it.
    pub rtt_ms: Option<u32>,
    /// Signed drift vs. this peer in ms. Positive means we are ahead of the
    /// peer; negative means we are behind. `None` if the peer has no
    /// heartbeat yet or we can't compare (e.g. one side is paused).
    pub drift_ms: Option<i64>,
    /// Whether the last heartbeat we received from this peer carried a
    /// `MediaId` that matches our own.
    pub media_match: Option<MatchResult>,
}
