//! Wire protocol for syncmesh.
//!
//! Every inter-peer message is a postcard-encoded `Frame`. The format is
//! position-based (postcard is not self-describing): field order and types in
//! this file are part of the wire contract and must not change without a
//! version bump.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::node_id::NodeId;

/// Top-level wire envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    Control(ControlEvent),
    Heartbeat(StateHeartbeat),
    Presence(PresenceEvent),
    Chat(ChatMessage),
}

impl Frame {
    /// Encode to postcard bytes.
    pub fn encode(&self) -> Vec<u8> {
        // `to_allocvec` only fails if the type hierarchy contains something
        // postcard can't represent (e.g. a map with non-string keys). All our
        // frame types are checked by the roundtrip tests, so a failure here is
        // a programming error and we surface it as such.
        postcard::to_allocvec(self).expect("frame types must always encode")
    }

    /// Decode from postcard bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        postcard::from_bytes(bytes).map_err(DecodeError::from)
    }
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("postcard decode failed: {0}")]
    Postcard(#[from] postcard::Error),
}

/// A reliably-ordered control event delivered on the per-pair control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlEvent {
    pub origin: NodeId,
    /// Originator's wall clock at the moment the event was emitted, in ms
    /// since the UNIX epoch. Used only with per-peer RTT compensation; there
    /// is no distributed clock agreement.
    pub origin_ts_ms: u64,
    /// Monotonic per-origin sequence number. Receivers dedup by `(origin, seq)`.
    pub seq: u64,
    pub action: ControlAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlAction {
    Play {
        media_pos_ms: u64,
    },
    Pause {
        media_pos_ms: u64,
    },
    Seek {
        media_pos_ms: u64,
    },
    /// Playback speed in centiunits — 100 == 1.00x.
    SetSpeed {
        speed_centi: u16,
    },
    MediaChanged {
        id: MediaId,
    },
}

/// Unreliable 1 Hz playback-state snapshot sent as a QUIC datagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateHeartbeat {
    pub origin: NodeId,
    pub origin_ts_ms: u64,
    pub media_pos_ms: u64,
    pub paused: bool,
    pub speed_centi: u16,
    pub ready: bool,
    pub media_id: Option<MediaId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresenceEvent {
    Join {
        node: NodeId,
        nickname: String,
    },
    Leave {
        node: NodeId,
    },
    Ready {
        node: NodeId,
        ready: bool,
    },
    Rename {
        node: NodeId,
        nickname: String,
    },
    /// Sent by an existing peer to a freshly joined peer so it can dial the
    /// rest of the mesh. The third tuple element is an opaque,
    /// transport-layer-encoded `EndpointAddr` for that peer — the core crate
    /// treats it as opaque bytes to preserve the crate split (core is
    /// iroh-free). Empty bytes are legal and mean "address not yet known";
    /// the receiver drops that entry for dialing purposes.
    PeerList {
        peers: Vec<(NodeId, String, Vec<u8>)>,
    },
    /// Broadcast by a peer once on connect so every other peer learns its
    /// current `EndpointAddr`. Opaque bytes per the same reasoning as
    /// `PeerList`.
    AddrAnnounce {
        node: NodeId,
        addr_bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub origin: NodeId,
    pub origin_ts_ms: u64,
    pub text: String,
}

/// Content-identity tuple used to detect "are we watching the same file?".
///
/// Per decision 11 in the plan, this is intentionally not a content hash:
/// hashing multi-GB files at startup is too slow. Mismatch is surfaced in the
/// UI but does not block playback.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaId {
    /// Lowercased filename (basename only, no path). Callers must lowercase
    /// before constructing — this type does not enforce the invariant at
    /// runtime because it participates in equality checks.
    pub filename_lower: String,
    pub size_bytes: u64,
    /// Duration in whole seconds, rounded.
    pub duration_s: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node() -> NodeId {
        NodeId::from_bytes([7u8; 32])
    }

    fn sample_media() -> MediaId {
        MediaId {
            filename_lower: "movie.mkv".into(),
            size_bytes: 1_234_567_890,
            duration_s: 7_200,
        }
    }

    fn assert_roundtrip(frame: &Frame) {
        let bytes = frame.encode();
        let decoded = Frame::decode(&bytes).expect("valid bytes must decode");
        assert_eq!(&decoded, frame);
    }

    #[test]
    fn control_play_roundtrips() {
        assert_roundtrip(&Frame::Control(ControlEvent {
            origin: sample_node(),
            origin_ts_ms: 1_000_000,
            seq: 42,
            action: ControlAction::Play {
                media_pos_ms: 5_500,
            },
        }));
    }

    #[test]
    fn control_pause_roundtrips() {
        assert_roundtrip(&Frame::Control(ControlEvent {
            origin: sample_node(),
            origin_ts_ms: 1,
            seq: 0,
            action: ControlAction::Pause { media_pos_ms: 0 },
        }));
    }

    #[test]
    fn control_seek_roundtrips() {
        assert_roundtrip(&Frame::Control(ControlEvent {
            origin: sample_node(),
            origin_ts_ms: u64::MAX / 2,
            seq: u64::MAX,
            action: ControlAction::Seek {
                media_pos_ms: 3_600_000,
            },
        }));
    }

    #[test]
    fn control_set_speed_roundtrips() {
        assert_roundtrip(&Frame::Control(ControlEvent {
            origin: sample_node(),
            origin_ts_ms: 1,
            seq: 1,
            action: ControlAction::SetSpeed { speed_centi: 95 },
        }));
    }

    #[test]
    fn control_media_changed_roundtrips() {
        assert_roundtrip(&Frame::Control(ControlEvent {
            origin: sample_node(),
            origin_ts_ms: 1,
            seq: 1,
            action: ControlAction::MediaChanged { id: sample_media() },
        }));
    }

    #[test]
    fn heartbeat_roundtrips_with_and_without_media() {
        let mut hb = StateHeartbeat {
            origin: sample_node(),
            origin_ts_ms: 100,
            media_pos_ms: 200,
            paused: false,
            speed_centi: 100,
            ready: true,
            media_id: Some(sample_media()),
        };
        assert_roundtrip(&Frame::Heartbeat(hb.clone()));
        hb.media_id = None;
        assert_roundtrip(&Frame::Heartbeat(hb));
    }

    #[test]
    fn presence_variants_all_roundtrip() {
        for p in [
            PresenceEvent::Join {
                node: sample_node(),
                nickname: "alice".into(),
            },
            PresenceEvent::Leave {
                node: sample_node(),
            },
            PresenceEvent::Ready {
                node: sample_node(),
                ready: true,
            },
            PresenceEvent::Rename {
                node: sample_node(),
                nickname: "bob".into(),
            },
            PresenceEvent::PeerList {
                peers: vec![
                    (sample_node(), "alice".into(), vec![]),
                    (
                        NodeId::from_bytes([9u8; 32]),
                        "carol".into(),
                        // Non-trivial opaque bytes: the core crate never
                        // inspects these, but we assert they round-trip.
                        (0u8..32).collect::<Vec<_>>(),
                    ),
                ],
            },
            PresenceEvent::AddrAnnounce {
                node: sample_node(),
                addr_bytes: (0u8..48).collect::<Vec<_>>(),
            },
        ] {
            assert_roundtrip(&Frame::Presence(p));
        }
    }

    #[test]
    fn chat_roundtrips_including_unicode() {
        assert_roundtrip(&Frame::Chat(ChatMessage {
            origin: sample_node(),
            origin_ts_ms: 1,
            text: "Hi 👋 — let's watch 🎬".into(),
        }));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Frame::decode(&[0xff; 8]).is_err());
        assert!(Frame::decode(&[]).is_err());
    }

    /// Control events are expected to be small. Serde's overhead is already
    /// tiny, but we assert an upper bound so regressions (e.g. adding a large
    /// field without noticing) are caught in tests rather than on the wire.
    #[test]
    fn control_event_stays_compact() {
        let frame = Frame::Control(ControlEvent {
            origin: sample_node(),
            origin_ts_ms: 1_700_000_000_000,
            seq: 1,
            action: ControlAction::Pause {
                media_pos_ms: 10_000,
            },
        });
        let bytes = frame.encode();
        assert!(bytes.len() <= 64, "control frame was {} bytes", bytes.len());
    }
}
