//! Echo suppression for mpv events.
//!
//! When the state machine applies an inbound `ControlAction` it emits an
//! `Output::Mpv(..)` command. mpv dutifully applies the command and then emits
//! a matching `MpvEvent` (a `Pause`, `TimePos`, or `Speed` edge). Without a
//! guard we'd re-broadcast that edge as if the local user had just pressed a
//! button, resulting in an event echo loop around the mesh.
//!
//! The guard is a short-lived "next matching edge is expected, eat it"
//! bookkeeping layer. Each time we dispatch an outbound `MpvCommand` we record
//! the expected next edge and a wall-clock deadline. The first matching edge
//! inside the window consumes the entry; any edge after the window expires
//! (or that doesn't match) is treated as user-initiated and broadcast.
//!
//! Seek tolerance: mpv snaps seeks to nearby keyframes, so the reported
//! `time-pos` after a commanded seek can differ from the target by a few
//! hundred milliseconds. For local-origin seeks we tolerate `SEEK_TOLERANCE_MS`
//! of slop and treat anything outside as a fresh user action. Pause and speed
//! are exact matches.
//!
//! Remote-origin seeks are different: when we dispatch an `MpvCommand::Seek`
//! because we just applied an inbound `ControlAction::Seek` from another peer,
//! the next post-`PlaybackRestart` `TimePos` is *guaranteed* to be ours, no
//! matter how far mpv's keyframe snap drifted from the requested target.
//! Re-broadcasting that edge causes a ping-pong loop between two peers whose
//! files have keyframes in slightly different places. Remote-origin entries
//! therefore consume unconditionally inside the window.

/// How long a pending echo entry remains valid before we give up and treat
/// the next matching edge as user-initiated. 3 s comfortably covers the
/// round-trip from dispatch-command → mpv → property-change-event even on
/// Windows named-pipe IPC under heavy seek-buffer flush, where the post-seek
/// `TimePos` has been observed to land later than the original 1.5 s budget.
const WINDOW_MS: u64 = 3_000;

/// Keyframe snap tolerance for echo-matching a commanded seek against mpv's
/// reported position after `PlaybackRestart`. Only used for local-origin
/// pending entries; remote-origin entries skip the tolerance check.
const SEEK_TOLERANCE_MS: u64 = 1_000;

/// Where the seek that armed the echo guard came from.
///
/// `Local` — a user keypress, drift correction, or any other locally-decided
/// command. We use the keyframe-snap tolerance to disambiguate "echo of our
/// command" from "user just keyed another seek before mpv replied".
///
/// `Remote` — we just dispatched a seek in response to an inbound peer
/// control event. The next seek edge from mpv is unambiguously ours; consume
/// it regardless of how far the position drifted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekOrigin {
    Local,
    Remote,
}

#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)]
pub struct EchoGuard {
    pending_pause: Option<PendingValue<bool>>,
    pending_seek: Option<PendingSeek>,
    pending_speed: Option<PendingValue<u16>>,
}

#[derive(Debug, Clone, Copy)]
struct PendingValue<T> {
    expected: T,
    deadline_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingSeek {
    expected: u64,
    deadline_ms: u64,
    origin: SeekOrigin,
}

impl EchoGuard {
    pub const fn new() -> Self {
        Self {
            pending_pause: None,
            pending_seek: None,
            pending_speed: None,
        }
    }

    /// Record that we just dispatched a `Pause(value)` to mpv; the next
    /// matching `MpvEvent::Pause` should be suppressed.
    pub fn record_pause(&mut self, value: bool, now_ms: u64) {
        self.pending_pause = Some(PendingValue {
            expected: value,
            deadline_ms: now_ms.saturating_add(WINDOW_MS),
        });
    }

    /// Record that we just dispatched a *local-origin* `Seek { media_pos_ms }`
    /// to mpv (user keypress, drift correction, etc.). The next matching
    /// `TimePos` edge within the keyframe-snap tolerance will be suppressed.
    pub fn record_seek(&mut self, media_pos_ms: u64, now_ms: u64) {
        self.pending_seek = Some(PendingSeek {
            expected: media_pos_ms,
            deadline_ms: now_ms.saturating_add(WINDOW_MS),
            origin: SeekOrigin::Local,
        });
    }

    /// Record that we just dispatched a *remote-origin* `Seek { media_pos_ms }`
    /// to mpv in response to an inbound peer control event. The next post-
    /// `PlaybackRestart` `TimePos` edge inside the window is consumed
    /// unconditionally, even if mpv snapped to a keyframe far from the target.
    pub fn record_seek_remote(&mut self, media_pos_ms: u64, now_ms: u64) {
        self.pending_seek = Some(PendingSeek {
            expected: media_pos_ms,
            deadline_ms: now_ms.saturating_add(WINDOW_MS),
            origin: SeekOrigin::Remote,
        });
    }

    /// Record that we just dispatched a `SetSpeed { speed_centi }` to mpv.
    pub fn record_speed(&mut self, speed_centi: u16, now_ms: u64) {
        self.pending_speed = Some(PendingValue {
            expected: speed_centi,
            deadline_ms: now_ms.saturating_add(WINDOW_MS),
        });
    }

    /// Check whether an incoming mpv `Pause(value)` edge matches a pending
    /// commanded pause within the window. Consumes the entry on a match.
    pub fn consume_pause(&mut self, value: bool, now_ms: u64) -> bool {
        let Some(p) = self.pending_pause else {
            return false;
        };
        if now_ms >= p.deadline_ms {
            self.pending_pause = None;
            return false;
        }
        if p.expected == value {
            self.pending_pause = None;
            true
        } else {
            false
        }
    }

    /// Check whether the post-seek `TimePos` matches a pending commanded
    /// seek. Local-origin entries match within `SEEK_TOLERANCE_MS` of the
    /// requested target; remote-origin entries match unconditionally inside
    /// the window. Consumes on match.
    pub fn consume_seek(&mut self, media_pos_ms: u64, now_ms: u64) -> bool {
        let Some(p) = self.pending_seek else {
            return false;
        };
        if now_ms >= p.deadline_ms {
            self.pending_seek = None;
            return false;
        }
        match p.origin {
            SeekOrigin::Remote => {
                self.pending_seek = None;
                true
            }
            SeekOrigin::Local => {
                if p.expected.abs_diff(media_pos_ms) <= SEEK_TOLERANCE_MS {
                    self.pending_seek = None;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Check whether an incoming mpv `Speed` edge matches a pending commanded
    /// speed change. Consumes on match.
    pub fn consume_speed(&mut self, speed_centi: u16, now_ms: u64) -> bool {
        let Some(p) = self.pending_speed else {
            return false;
        };
        if now_ms >= p.deadline_ms {
            self.pending_speed = None;
            return false;
        }
        if p.expected == speed_centi {
            self.pending_speed = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_echo_within_window_is_consumed() {
        let mut g = EchoGuard::new();
        g.record_pause(true, 1_000);
        assert!(g.consume_pause(true, 1_100));
        // Second call should miss — the entry was consumed.
        assert!(!g.consume_pause(true, 1_200));
    }

    #[test]
    fn pause_edge_with_wrong_value_does_not_consume() {
        let mut g = EchoGuard::new();
        g.record_pause(true, 1_000);
        assert!(!g.consume_pause(false, 1_100));
        // Entry still pending: a later correct edge within window still matches.
        assert!(g.consume_pause(true, 1_200));
    }

    #[test]
    fn pause_edge_after_deadline_expires_entry() {
        let mut g = EchoGuard::new();
        g.record_pause(true, 1_000);
        // Well past the suppression window.
        assert!(!g.consume_pause(true, 1_000 + WINDOW_MS));
        // Entry cleared; a future record+consume works normally.
        g.record_pause(true, 5_000);
        assert!(g.consume_pause(true, 5_500));
    }

    #[test]
    fn seek_echo_within_tolerance_is_consumed() {
        let mut g = EchoGuard::new();
        g.record_seek(10_000, 100);
        // Keyframe snap: mpv lands 300 ms short of target.
        assert!(g.consume_seek(9_700, 200));
    }

    #[test]
    fn local_seek_beyond_tolerance_does_not_consume() {
        let mut g = EchoGuard::new();
        g.record_seek(10_000, 100);
        assert!(!g.consume_seek(12_000, 200));
        // Still pending — a within-tolerance call still works.
        assert!(g.consume_seek(10_500, 300));
    }

    #[test]
    fn remote_seek_beyond_tolerance_is_consumed() {
        // Remote-origin seeks must consume the next post-PlaybackRestart
        // TimePos no matter how far mpv's keyframe snap drifted, because
        // re-broadcasting it would cause a 2-peer ping-pong loop.
        let mut g = EchoGuard::new();
        g.record_seek_remote(10_000, 100);
        // 40 s of "drift" — would never match local-origin tolerance, but the
        // remote-origin guarantee ("the next seek edge is unambiguously ours")
        // means we still consume.
        assert!(g.consume_seek(50_000, 200));
        // Entry cleared after consume.
        assert!(!g.consume_seek(50_000, 300));
    }

    #[test]
    fn remote_seek_still_expires_at_deadline() {
        let mut g = EchoGuard::new();
        g.record_seek_remote(10_000, 100);
        assert!(!g.consume_seek(10_000, 100 + WINDOW_MS));
        // Entry cleared; a future record+consume works normally.
        g.record_seek_remote(20_000, 5_000);
        assert!(g.consume_seek(99_999, 5_500));
    }

    #[test]
    fn record_seek_overrides_origin_of_prior_entry() {
        // record_seek (Local) replaces a prior remote-origin entry; the next
        // edge is checked with tolerance again. Symmetric the other way.
        let mut g = EchoGuard::new();
        g.record_seek_remote(10_000, 100);
        g.record_seek(20_000, 110);
        // Now Local — far-off TimePos should NOT consume.
        assert!(!g.consume_seek(99_999, 200));
        // Pending entry is still the local one; an in-tolerance call consumes.
        assert!(g.consume_seek(20_300, 300));
    }

    #[test]
    fn speed_exact_match_consumes() {
        let mut g = EchoGuard::new();
        g.record_speed(95, 0);
        assert!(!g.consume_speed(100, 1));
        assert!(g.consume_speed(95, 2));
    }

    #[test]
    fn independent_categories_do_not_interfere() {
        let mut g = EchoGuard::new();
        g.record_pause(true, 0);
        g.record_seek(5_000, 0);
        g.record_speed(95, 0);
        assert!(g.consume_pause(true, 1));
        assert!(g.consume_seek(5_000, 2));
        assert!(g.consume_speed(95, 3));
    }
}
