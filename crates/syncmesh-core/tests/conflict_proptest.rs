//! Property-based tests for the conflict resolver.
//!
//! The protocol's convergence guarantee leans on `resolve()` being a total,
//! symmetric, transitive ordering over events in a 500 ms window. If these
//! properties regress, peers in a mesh will diverge silently.

use proptest::prelude::*;
use syncmesh_core::{
    CONFLICT_WINDOW_MS, ConflictOutcome, ControlAction, ControlEvent, NodeId, resolve,
};

fn any_action() -> impl Strategy<Value = ControlAction> {
    // Only generate same-kind actions for most properties — different kinds
    // trivially compose. Individual tests narrow further.
    prop_oneof![
        any::<u64>().prop_map(|p| ControlAction::Pause { media_pos_ms: p }),
        any::<u64>().prop_map(|p| ControlAction::Play { media_pos_ms: p }),
        any::<u64>().prop_map(|p| ControlAction::Seek { media_pos_ms: p }),
    ]
}

fn any_pause() -> impl Strategy<Value = ControlAction> {
    any::<u64>().prop_map(|p| ControlAction::Pause { media_pos_ms: p })
}

fn any_event(action: impl Strategy<Value = ControlAction>) -> impl Strategy<Value = ControlEvent> {
    (any::<[u8; 32]>(), any::<u64>(), any::<u64>(), action).prop_map(|(bytes, ts, seq, action)| {
        ControlEvent {
            origin: NodeId::from_bytes(bytes),
            origin_ts_ms: ts,
            seq,
            action,
        }
    })
}

proptest! {
    /// `resolve` is deterministic.
    #[test]
    fn resolve_is_deterministic(
        a in any_event(any_action()),
        b in any_event(any_action()),
    ) {
        prop_assert_eq!(resolve(&a, &b), resolve(&a, &b));
    }

    /// Different action kinds never conflict.
    #[test]
    fn different_action_kinds_always_apply(
        a in any_event(any::<u64>().prop_map(|p| ControlAction::Pause { media_pos_ms: p })),
        b in any_event(any::<u64>().prop_map(|p| ControlAction::Seek { media_pos_ms: p })),
    ) {
        prop_assert_eq!(resolve(&a, &b), ConflictOutcome::Apply);
        prop_assert_eq!(resolve(&b, &a), ConflictOutcome::Apply);
    }

    /// Outside the window, same-kind events compose freely.
    #[test]
    fn outside_window_always_applies(
        a in any_event(any_pause()),
        mut b in any_event(any_pause()),
        offset in CONFLICT_WINDOW_MS..(CONFLICT_WINDOW_MS * 1_000_000),
    ) {
        // Push b's timestamp clearly outside the window, keeping it in-range.
        b.origin_ts_ms = a.origin_ts_ms.saturating_add(offset);
        prop_assert_eq!(resolve(&a, &b), ConflictOutcome::Apply);
        prop_assert_eq!(resolve(&b, &a), ConflictOutcome::Apply);
    }

    /// Anti-symmetry inside the window: if `resolve(a, b) = Discard`, then
    /// `resolve(b, a)` must be `Apply` (and vice-versa for distinct events).
    /// This is the property that makes mesh convergence possible.
    #[test]
    fn in_window_resolution_is_antisymmetric(
        bytes_a in any::<[u8; 32]>(),
        bytes_b in any::<[u8; 32]>(),
        base_ts in 0u64..u64::MAX / 2,
        offset in 0u64..CONFLICT_WINDOW_MS,
        pos_a in any::<u64>(),
        pos_b in any::<u64>(),
    ) {
        let a = ControlEvent {
            origin: NodeId::from_bytes(bytes_a),
            origin_ts_ms: base_ts,
            seq: 0,
            action: ControlAction::Pause { media_pos_ms: pos_a },
        };
        let b = ControlEvent {
            origin: NodeId::from_bytes(bytes_b),
            origin_ts_ms: base_ts + offset,
            seq: 0,
            action: ControlAction::Pause { media_pos_ms: pos_b },
        };
        prop_assume!((a.origin, a.origin_ts_ms) != (b.origin, b.origin_ts_ms));

        let ab = resolve(&a, &b);
        let ba = resolve(&b, &a);
        // Exactly one of the two pairs must Discard, the other must Apply.
        let discards = [ab, ba].iter().filter(|o| **o == ConflictOutcome::Discard).count();
        prop_assert_eq!(discards, 1, "got ab={:?} ba={:?}", ab, ba);
    }

    /// Identical events (same origin, ts, seq) are not a conflict — dedup is
    /// the seq-counter's job.
    #[test]
    fn identical_events_apply(ev in any_event(any_action())) {
        prop_assert_eq!(resolve(&ev, &ev), ConflictOutcome::Apply);
    }
}
