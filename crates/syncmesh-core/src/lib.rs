//! Pure-logic core for syncmesh.
//!
//! This crate contains the wire protocol, the sync state machine, and the
//! algorithms that decide what to do with peer heartbeats and control events.
//! It performs no I/O and has no dependency on iroh, mpv, or tokio — every
//! function is a pure transform from `(state, event) -> (new_state, outputs)`
//! so it can be exhaustively tested with simulated time.

pub mod conflict;
pub mod drift;
pub mod media;
pub mod node_id;
pub mod protocol;
pub mod ready;
pub mod rtt;
pub mod state;
pub mod time;

pub use conflict::{CONFLICT_WINDOW_MS, ConflictOutcome, resolve};
pub use drift::{DRIFT_HARD_SEEK_MS, DRIFT_SLOWDOWN_MS, DriftAction, check_drift};
pub use media::{MatchResult, match_media};
pub use node_id::NodeId;
pub use protocol::{
    ChatMessage, ControlAction, ControlEvent, DecodeError, Frame, MediaId, PresenceEvent,
    StateHeartbeat,
};
pub use ready::{ReadyGate, ReadyState};
pub use rtt::RttEstimator;
pub use state::{
    Input, MpvCommand, Notice, Output, PeerState, PlaybackState, RoomState, SPEED_DOWN_CENTI,
    SPEED_NORMAL_CENTI, SPEED_UP_CENTI,
};
pub use time::{Clock, MockClock, SystemClock};
