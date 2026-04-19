//! Drift correction calculator.
//!
//! Per plan decision 14, each peer runs this pure function once per second
//! against a reference peer (typically whichever remote peer has the freshest
//! heartbeat). The result tells the event loop whether to issue an mpv
//! `speed` change or a hard seek.
//!
//! Thresholds:
//!   - `|diff|  >= 1000 ms` → hard seek to the projected reference position
//!   -  diff   >=  100 ms   → I'm ahead of the reference: slow down
//!   -  diff   <= -100 ms   → I'm behind the reference: speed up
//!   - otherwise no action
//!
//! Note: the plan's §2.2 pseudocode labels these the other way round, which
//! would make drift diverge rather than converge. The thresholds and overall
//! approach match the plan; the slow-down-if-ahead convention matches Syncplay
//! and is what actually converges.

/// Threshold above which we hard-seek rather than adjust speed, in ms.
pub const DRIFT_HARD_SEEK_MS: i64 = 1_000;

/// Threshold above which we adjust playback speed, in ms.
pub const DRIFT_SLOWDOWN_MS: i64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftAction {
    None,
    /// I am ahead of the reference. Reduce my playback speed (e.g. 0.95x) so I
    /// fall back toward the reference.
    Slowdown,
    /// I am behind the reference. Increase my playback speed (e.g. 1.05x) to
    /// catch up.
    Speedup,
    /// `|diff|` exceeds the hard-seek threshold. Jump directly to the
    /// projected reference position.
    Seek {
        target_ms: u64,
    },
}

/// Snapshot of a remote peer's state at the moment its last heartbeat was
/// received. `origin_ts_ms` is the peer's wall clock at emission; we trust it
/// and compensate per-peer via `one_way_ms`.
#[derive(Debug, Clone, Copy)]
pub struct ReferencePeer {
    pub media_pos_ms: u64,
    pub origin_ts_ms: u64,
    pub paused: bool,
    /// Half the current RTT EWMA to this peer.
    pub one_way_ms: u32,
}

/// Our own local playback state.
#[derive(Debug, Clone, Copy)]
pub struct LocalPlayback {
    pub media_pos_ms: u64,
    pub paused: bool,
}

/// Project where the reference peer is *now*, given its last heartbeat and the
/// current wall clock, accounting for network one-way latency.
///
/// Returned as a signed `i64` so callers can detect underflow (e.g. a remote
/// peer reporting a timestamp in the future) rather than silently wrapping.
/// `check_drift` clamps the result to `>= 0` before use.
pub fn project_reference(reference: &ReferencePeer, now_ms: u64) -> i64 {
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let origin_ts = i64::try_from(reference.origin_ts_ms).unwrap_or(0);
    let elapsed = now - origin_ts;
    let pos = i64::try_from(reference.media_pos_ms).unwrap_or(i64::MAX);
    pos + elapsed + i64::from(reference.one_way_ms)
}

/// Decide the drift action for this local tick.
///
/// Returns `DriftAction::None` whenever:
/// - The two paused states disagree (resolving that is a control-event problem,
///   not a drift problem).
/// - Both peers are paused.
/// - The projected positions are within the ±100 ms deadband.
pub fn check_drift(reference: &ReferencePeer, local: &LocalPlayback, now_ms: u64) -> DriftAction {
    if reference.paused != local.paused {
        return DriftAction::None;
    }
    if local.paused {
        return DriftAction::None;
    }
    let projected = project_reference(reference, now_ms).max(0);
    let my_pos = i64::try_from(local.media_pos_ms).unwrap_or(i64::MAX);
    let diff = my_pos - projected; // positive = I'm ahead

    if diff.abs() >= DRIFT_HARD_SEEK_MS {
        // `projected` is clamped to `>= 0` above, so the conversion is infallible,
        // but using try_from gives clippy the proof.
        return DriftAction::Seek {
            target_ms: u64::try_from(projected).unwrap_or(0),
        };
    }
    if diff >= DRIFT_SLOWDOWN_MS {
        return DriftAction::Slowdown; // ahead → slow my clock
    }
    if diff <= -DRIFT_SLOWDOWN_MS {
        return DriftAction::Speedup; // behind → speed my clock up
    }
    DriftAction::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_at(pos_ms: u64, ts_ms: u64, one_way: u32) -> ReferencePeer {
        ReferencePeer {
            media_pos_ms: pos_ms,
            origin_ts_ms: ts_ms,
            paused: false,
            one_way_ms: one_way,
        }
    }

    fn me_at(pos_ms: u64) -> LocalPlayback {
        LocalPlayback {
            media_pos_ms: pos_ms,
            paused: false,
        }
    }

    #[test]
    fn exactly_in_sync_emits_no_action() {
        let r = ref_at(10_000, 1_000, 0);
        let me = me_at(10_000);
        assert_eq!(check_drift(&r, &me, 1_000), DriftAction::None);
    }

    #[test]
    fn within_deadband_emits_no_action() {
        let r = ref_at(10_000, 1_000, 0);
        let me_ahead = me_at(10_099);
        let me_behind = me_at(9_901);
        assert_eq!(check_drift(&r, &me_ahead, 1_000), DriftAction::None);
        assert_eq!(check_drift(&r, &me_behind, 1_000), DriftAction::None);
    }

    #[test]
    fn i_am_behind_triggers_speedup() {
        // my_pos (9_850) < ref (10_000) → diff = -150 → Speedup.
        let r = ref_at(10_000, 1_000, 0);
        let me = me_at(9_850);
        assert_eq!(check_drift(&r, &me, 1_000), DriftAction::Speedup);
    }

    #[test]
    fn i_am_ahead_triggers_slowdown() {
        // my_pos (10_150) > ref (10_000) → diff = +150 → Slowdown.
        let r = ref_at(10_000, 1_000, 0);
        let me = me_at(10_150);
        assert_eq!(check_drift(&r, &me, 1_000), DriftAction::Slowdown);
    }

    #[test]
    fn hard_lag_triggers_seek_forward() {
        // 1500 ms behind → hard seek to reference.
        let r = ref_at(10_000, 1_000, 0);
        let me = me_at(8_500);
        let action = check_drift(&r, &me, 1_000);
        assert!(matches!(action, DriftAction::Seek { target_ms: 10_000 }));
    }

    #[test]
    fn hard_lead_triggers_seek_back() {
        // 2000 ms ahead → hard seek to reference.
        let r = ref_at(10_000, 1_000, 0);
        let me = me_at(12_000);
        let action = check_drift(&r, &me, 1_000);
        assert!(matches!(action, DriftAction::Seek { target_ms: 10_000 }));
    }

    #[test]
    fn hard_threshold_is_inclusive() {
        // Exactly 1000 ms ahead should trigger Seek.
        let r = ref_at(10_000, 1_000, 0);
        let me = me_at(11_000);
        assert!(matches!(
            check_drift(&r, &me, 1_000),
            DriftAction::Seek { .. }
        ));
    }

    #[test]
    fn deadband_boundary_is_exactly_100_ms() {
        // diff = +100 → Slowdown (inclusive).
        let r = ref_at(10_000, 1_000, 0);
        let me_100_ahead = me_at(10_100);
        assert_eq!(check_drift(&r, &me_100_ahead, 1_000), DriftAction::Slowdown);
        // diff = -100 → Speedup (inclusive).
        let me_100_behind = me_at(9_900);
        assert_eq!(check_drift(&r, &me_100_behind, 1_000), DriftAction::Speedup);
    }

    #[test]
    fn projection_accounts_for_elapsed_time() {
        // Reference heartbeat said pos=10_000 at ts=1_000. Now it's 1_500 ms,
        // so reference projects to 10_500 with one_way=0. If I'm at 10_500,
        // we're in sync.
        let r = ref_at(10_000, 1_000, 0);
        let me = me_at(10_500);
        assert_eq!(check_drift(&r, &me, 1_500), DriftAction::None);
    }

    #[test]
    fn projection_adds_one_way_compensation() {
        // Heartbeat traversed 30 ms one-way. Add that to projected position.
        let r = ref_at(10_000, 1_000, 30);
        let me = me_at(10_530);
        assert_eq!(check_drift(&r, &me, 1_500), DriftAction::None);
    }

    #[test]
    fn both_paused_returns_none() {
        let r = ReferencePeer {
            media_pos_ms: 10_000,
            origin_ts_ms: 1_000,
            paused: true,
            one_way_ms: 0,
        };
        let me = LocalPlayback {
            media_pos_ms: 9_000,
            paused: true,
        };
        assert_eq!(check_drift(&r, &me, 1_000), DriftAction::None);
    }

    #[test]
    fn mismatched_pause_returns_none() {
        let mut r = ref_at(10_000, 1_000, 0);
        r.paused = true;
        let me = me_at(9_000);
        assert_eq!(check_drift(&r, &me, 1_000), DriftAction::None);
    }

    #[test]
    fn projected_negative_position_is_clamped() {
        // Pathological: origin_ts is far in the future. Projection goes
        // negative, gets clamped to 0. Assertion: no panic, no surprising
        // wraparound, and the clamped value is used for the diff.
        let r = ref_at(0, 10_000, 0);
        let me = me_at(50);
        // projected = 0 + (1000 - 10000) = -9000, clamp → 0. diff = 50 - 0 = 50
        // → within deadband → None.
        assert_eq!(check_drift(&r, &me, 1_000), DriftAction::None);
    }

    #[test]
    fn seek_target_is_projected_position_not_raw() {
        // If reference is stale (heartbeat from 2s ago), seek should target
        // the *projected* position, not the raw media_pos from the heartbeat.
        let r = ref_at(10_000, 0, 0);
        let me = me_at(5_000); // way behind
        let action = check_drift(&r, &me, 2_000);
        // projected = 10_000 + (2_000 - 0) = 12_000
        assert_eq!(action, DriftAction::Seek { target_ms: 12_000 });
    }
}
