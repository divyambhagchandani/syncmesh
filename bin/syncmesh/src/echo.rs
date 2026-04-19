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
//! hundred milliseconds. We tolerate `SEEK_TOLERANCE_MS` of slop. Pause and
//! speed are exact matches.

/// How long a pending echo entry remains valid before we give up and treat
/// the next matching edge as user-initiated. 1.5 s comfortably covers the
/// round-trip from dispatch-command → mpv → property-change-event even when
/// the IPC socket is under load.
const WINDOW_MS: u64 = 1_500;

/// Keyframe snap tolerance for echo-matching a commanded seek against mpv's
/// reported position after `PlaybackRestart`.
const SEEK_TOLERANCE_MS: u64 = 1_000;

#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)]
pub struct EchoGuard {
    pending_pause: Option<PendingValue<bool>>,
    pending_seek: Option<PendingValue<u64>>,
    pending_speed: Option<PendingValue<u16>>,
}

#[derive(Debug, Clone, Copy)]
struct PendingValue<T> {
    expected: T,
    deadline_ms: u64,
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

    /// Record that we just dispatched a `Seek { media_pos_ms }` to mpv.
    pub fn record_seek(&mut self, media_pos_ms: u64, now_ms: u64) {
        self.pending_seek = Some(PendingValue {
            expected: media_pos_ms,
            deadline_ms: now_ms.saturating_add(WINDOW_MS),
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
    /// seek target (within `SEEK_TOLERANCE_MS`). Consumes on match.
    pub fn consume_seek(&mut self, media_pos_ms: u64, now_ms: u64) -> bool {
        let Some(p) = self.pending_seek else {
            return false;
        };
        if now_ms >= p.deadline_ms {
            self.pending_seek = None;
            return false;
        }
        if p.expected.abs_diff(media_pos_ms) <= SEEK_TOLERANCE_MS {
            self.pending_seek = None;
            true
        } else {
            false
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
        // Well past the 1.5 s window.
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
    fn seek_echo_beyond_tolerance_does_not_consume() {
        let mut g = EchoGuard::new();
        g.record_seek(10_000, 100);
        assert!(!g.consume_seek(12_000, 200));
        // Still pending — a within-tolerance call still works.
        assert!(g.consume_seek(10_500, 300));
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
