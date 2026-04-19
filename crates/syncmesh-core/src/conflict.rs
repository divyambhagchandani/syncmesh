//! Deterministic conflict resolver for simultaneous control events.
//!
//! When two peers emit the same kind of control event at nearly the same
//! instant (the canonical case: two users pressing pause at once), every peer
//! in the mesh must pick the same one as canonical or the room desyncs. Plan
//! decision 8 mandates a `(origin_ts, origin_id)` lexicographic tiebreaker,
//! with a 500 ms conflict window outside of which events compose freely.

use core::mem;

use crate::protocol::ControlEvent;

/// Events with originator timestamps within this window are considered a
/// conflict if they are of the same kind. Plan §2.2.
pub const CONFLICT_WINDOW_MS: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictOutcome {
    /// The incoming event should be applied.
    Apply,
    /// The incoming event is superseded by a prior event with a smaller
    /// `(origin_ts, origin_id)` tuple and should be discarded.
    Discard,
}

/// Decide whether to apply `incoming` given a recently-applied `prior` event.
///
/// - Different kinds of action (e.g. Pause vs Seek) never conflict — one
///   composes with the other. `Apply`.
/// - Same kind of action outside the window — also `Apply`.
/// - Same kind of action inside the window — the smaller
///   `(origin_ts_ms, origin)` tuple wins. `Discard` the incoming event iff it
///   has the *larger* tuple.
pub fn resolve(prior: &ControlEvent, incoming: &ControlEvent) -> ConflictOutcome {
    if mem::discriminant(&prior.action) != mem::discriminant(&incoming.action) {
        return ConflictOutcome::Apply;
    }
    if prior.origin_ts_ms.abs_diff(incoming.origin_ts_ms) >= CONFLICT_WINDOW_MS {
        return ConflictOutcome::Apply;
    }
    let prior_tuple = (prior.origin_ts_ms, prior.origin);
    let incoming_tuple = (incoming.origin_ts_ms, incoming.origin);
    if incoming_tuple > prior_tuple {
        ConflictOutcome::Discard
    } else {
        ConflictOutcome::Apply
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_id::NodeId;
    use crate::protocol::ControlAction;

    fn node(b: u8) -> NodeId {
        NodeId::from_bytes([b; 32])
    }

    fn pause(origin: NodeId, ts: u64, pos: u64) -> ControlEvent {
        ControlEvent {
            origin,
            origin_ts_ms: ts,
            seq: 0,
            action: ControlAction::Pause { media_pos_ms: pos },
        }
    }

    fn play(origin: NodeId, ts: u64, pos: u64) -> ControlEvent {
        ControlEvent {
            origin,
            origin_ts_ms: ts,
            seq: 0,
            action: ControlAction::Play { media_pos_ms: pos },
        }
    }

    fn seek(origin: NodeId, ts: u64, pos: u64) -> ControlEvent {
        ControlEvent {
            origin,
            origin_ts_ms: ts,
            seq: 0,
            action: ControlAction::Seek { media_pos_ms: pos },
        }
    }

    #[test]
    fn different_action_kinds_never_conflict() {
        // Pause and Seek at the same instant should both apply: you can pause
        // then seek.
        let a = pause(node(1), 1000, 100);
        let b = seek(node(2), 1000, 500);
        assert_eq!(resolve(&a, &b), ConflictOutcome::Apply);
        assert_eq!(resolve(&b, &a), ConflictOutcome::Apply);
    }

    #[test]
    fn same_kind_outside_window_applies() {
        let a = pause(node(1), 1000, 100);
        let b = pause(node(2), 1500, 200); // exactly at the window boundary
        assert_eq!(resolve(&a, &b), ConflictOutcome::Apply);
        let c = pause(node(2), 1600, 200);
        assert_eq!(resolve(&a, &c), ConflictOutcome::Apply);
    }

    #[test]
    fn same_kind_inside_window_smaller_tuple_wins() {
        let prior = pause(node(1), 1000, 100);
        let incoming = pause(node(2), 1100, 200);
        // prior has smaller ts (1000 < 1100), so incoming should be discarded.
        assert_eq!(resolve(&prior, &incoming), ConflictOutcome::Discard);
    }

    #[test]
    fn same_kind_inside_window_later_event_with_smaller_id_wins() {
        // prior ts = 1100, incoming ts = 1000 — incoming is earlier. Incoming
        // should win (smaller tuple on the primary key) and be applied.
        let prior = pause(node(2), 1100, 100);
        let incoming = pause(node(1), 1000, 200);
        assert_eq!(resolve(&prior, &incoming), ConflictOutcome::Apply);
    }

    #[test]
    fn tie_on_timestamp_resolved_by_node_id() {
        // Same timestamp — node id breaks the tie. Lower id wins.
        let prior_low = pause(node(1), 1000, 100);
        let incoming_high = pause(node(2), 1000, 200);
        assert_eq!(
            resolve(&prior_low, &incoming_high),
            ConflictOutcome::Discard
        );

        let prior_high = pause(node(2), 1000, 200);
        let incoming_low = pause(node(1), 1000, 100);
        assert_eq!(resolve(&prior_high, &incoming_low), ConflictOutcome::Apply);
    }

    #[test]
    fn identical_tuple_applies() {
        // A peer replaying its own prior event is not a conflict — dedup is
        // handled by the per-origin seq, not here.
        let a = pause(node(1), 1000, 100);
        let b = pause(node(1), 1000, 100);
        assert_eq!(resolve(&a, &b), ConflictOutcome::Apply);
    }

    #[test]
    fn play_vs_play_same_kind() {
        let a = play(node(1), 1000, 100);
        let b = play(node(2), 1100, 100);
        assert_eq!(resolve(&a, &b), ConflictOutcome::Discard);
    }

    #[test]
    fn abs_diff_handles_reversed_order() {
        // incoming is temporally before prior — same window semantics.
        let prior = pause(node(2), 1500, 100);
        let incoming = pause(node(3), 1100, 200);
        // Window is 400ms < 500ms, same kind — smaller tuple wins.
        // Incoming ts = 1100 < prior ts = 1500, so incoming wins → Apply.
        assert_eq!(resolve(&prior, &incoming), ConflictOutcome::Apply);
    }

    #[test]
    fn deterministic_across_swap_pairs() {
        // Property we need for mesh consistency: for any two same-kind events in
        // the same window, exactly one "wins" in a way all peers agree on. Check
        // that resolve() is consistent when called in either order.
        let x = pause(node(3), 1000, 100);
        let y = pause(node(7), 1050, 200);
        // If a peer first sees x then y, it should Discard y.
        assert_eq!(resolve(&x, &y), ConflictOutcome::Discard);
        // If a peer first sees y then x, it should Apply x (smaller tuple wins).
        assert_eq!(resolve(&y, &x), ConflictOutcome::Apply);
    }
}
