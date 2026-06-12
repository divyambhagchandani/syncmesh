//! Regression test for the 10-second seek ping-pong between two playing peers.
//!
//! Reported scenario: both peers play the same file in sync; the user on peer
//! A presses `→` in mpv (+10 s). The seek control event propagates correctly,
//! but A's 1 Hz drift tick still holds B's *pre-seek* heartbeat (≤1 s stale,
//! reporting the old position). The 10 s diff exceeds `DRIFT_HARD_SEEK_MS`,
//! so A's drift corrector hard-seeks A back — silently undoing the user's
//! seek (drift seeks are echo-consumed, never broadcast). From then on each
//! peer only ever sees the other's ~1 s-old position and hard-seeks toward
//! it every tick: a sustained ±10 s oscillation that only pausing breaks.
//!
//! The fix is twofold (see `RoomState`):
//!   - a `DRIFT_HOLDOFF_MS` suspension of drift correction after any
//!     position-carrying control event (sent or applied), plus refusing to
//!     use heartbeats received at or before that event as a reference;
//!   - a hard-seek tie-break so only the higher-`NodeId` peer of a pair ever
//!     hard-seeks, making mutual stale-position correction impossible.
//!
//! This test simulates the full two-peer loop deterministically: simulated
//! wall clock, 1 Hz ticks, instant frame delivery, and a model mpv that
//! applies `MpvCommand`s back into each state via `MpvStateUpdate` (the
//! echo-guard-consumed path — drift seeks are not re-broadcast).

use syncmesh_core::{
    ControlAction, Input, MpvCommand, NodeId, Output, RoomState, SPEED_NORMAL_CENTI,
};

fn node(b: u8) -> NodeId {
    NodeId::from_bytes([b; 32])
}

/// One simulated peer: a `RoomState` plus a model of its mpv player.
struct SimPeer {
    state: RoomState,
    pos_ms: u64,
    speed_centi: u16,
    /// `MpvCommand::Seek`s this peer's drift/control handling issued.
    seeks_applied: Vec<u64>,
}

impl SimPeer {
    fn new(id: NodeId, nick: &str) -> Self {
        Self {
            state: RoomState::new(id, nick.into()),
            pos_ms: 0,
            speed_centi: SPEED_NORMAL_CENTI,
            seeks_applied: Vec::new(),
        }
    }

    /// Advance the model player by `dt_ms` of wall time and refresh the
    /// state's view of it (the passive `TimePos` path).
    fn advance(&mut self, dt_ms: u64) {
        self.pos_ms += dt_ms * u64::from(self.speed_centi) / 100;
        let _ = self.state.apply(Input::MpvStateUpdate {
            media_pos_ms: self.pos_ms,
            paused: false,
            speed_centi: self.speed_centi,
        });
    }

    /// Apply `MpvCommand`s to the model player and mirror the result back
    /// into the state, exactly like the bin's echo-guarded dispatch: the
    /// resulting mpv edge becomes an `MpvStateUpdate`, never a re-broadcast.
    fn run_mpv(&mut self, outs: &[Output]) {
        for out in outs {
            if let Output::Mpv(cmd) = out {
                match cmd {
                    MpvCommand::Seek { media_pos_ms } => {
                        self.pos_ms = *media_pos_ms;
                        self.seeks_applied.push(*media_pos_ms);
                    }
                    MpvCommand::SetSpeed { speed_centi } => self.speed_centi = *speed_centi,
                    MpvCommand::Pause(_) => {}
                }
            }
        }
        let _ = self.state.apply(Input::MpvStateUpdate {
            media_pos_ms: self.pos_ms,
            paused: false,
            speed_centi: self.speed_centi,
        });
    }
}

/// Deliver every broadcast frame in `outs` from `src` to `dst` (instant,
/// zero-latency link), apply any resulting mpv commands on `dst`, and return
/// nothing — second-order outputs of `dst` are broadcasts of its own and are
/// handled by its next tick.
fn deliver(outs: &[Output], dst: &mut SimPeer, src_node: NodeId, now_ms: u64) {
    for out in outs {
        if let Output::Broadcast(frame) = out {
            let dst_outs = dst.state.apply(Input::FrameReceived {
                from: src_node,
                frame: frame.clone(),
                received_at_ms: now_ms,
                rtt_sample_ms: None,
            });
            dst.run_mpv(&dst_outs);
        }
    }
}

#[test]
fn forward_seek_while_playing_does_not_oscillate() {
    let id_a = node(1);
    let id_b = node(2);
    let mut a = SimPeer::new(id_a, "alice");
    let mut b = SimPeer::new(id_b, "bob");

    let _ = a.state.apply(Input::PeerConnected {
        node: id_b,
        nickname: "bob".into(),
    });
    let _ = b.state.apply(Input::PeerConnected {
        node: id_a,
        nickname: "alice".into(),
    });

    // Both playing in sync at 60 s.
    a.pos_ms = 60_000;
    b.pos_ms = 60_000;
    a.advance(0);
    b.advance(0);

    let mut now: u64 = 1_000;
    let step_ms: u64 = 250;

    // Warm up: 5 simulated seconds of playback with 1 Hz ticks exchanging
    // heartbeats. No corrections expected while in sync.
    for _ in 0..20 {
        a.advance(step_ms);
        b.advance(step_ms);
        now += step_ms;
        if now % 1_000 == 0 {
            let outs_a = a.state.apply(Input::Tick { now_ms: now });
            a.run_mpv(&outs_a);
            deliver(&outs_a, &mut b, id_a, now);
            let outs_b = b.state.apply(Input::Tick { now_ms: now });
            b.run_mpv(&outs_b);
            deliver(&outs_b, &mut a, id_b, now);
        }
    }
    assert!(
        a.seeks_applied.is_empty() && b.seeks_applied.is_empty(),
        "no corrections expected while in sync"
    );

    // Mid-tick (heartbeats are now up to 1 s stale): the user on A presses
    // `→`. mpv seeks +10 s and the bin broadcasts it as a local Seek.
    a.advance(step_ms);
    b.advance(step_ms);
    now += step_ms;
    a.pos_ms += 10_000;
    let seek_target = a.pos_ms;
    let outs = a.state.apply(Input::LocalControl {
        action: ControlAction::Seek {
            media_pos_ms: seek_target,
        },
        now_ms: now,
    });
    a.run_mpv(&outs);
    deliver(&outs, &mut b, id_a, now);
    // B's mpv applied the inbound seek (the one legitimate seek).
    assert_eq!(b.seeks_applied, vec![seek_target]);
    a.seeks_applied.clear();
    b.seeks_applied.clear();

    // Run 8 more simulated seconds. Pre-fix, A's first tick hard-seeks back
    // to B's stale pre-seek position and the two peers swap places forever.
    for _ in 0..32 {
        a.advance(step_ms);
        b.advance(step_ms);
        now += step_ms;
        if now % 1_000 == 0 {
            let outs_a = a.state.apply(Input::Tick { now_ms: now });
            a.run_mpv(&outs_a);
            deliver(&outs_a, &mut b, id_a, now);
            let outs_b = b.state.apply(Input::Tick { now_ms: now });
            b.run_mpv(&outs_b);
            deliver(&outs_b, &mut a, id_b, now);
        }
    }

    assert!(
        a.seeks_applied.is_empty(),
        "peer A must not drift-seek after its own forward seek; got {:?}",
        a.seeks_applied
    );
    assert!(
        b.seeks_applied.is_empty(),
        "peer B must not drift-seek after applying the inbound seek; got {:?}",
        b.seeks_applied
    );
    let diff = a.pos_ms.abs_diff(b.pos_ms);
    assert!(
        diff <= 500,
        "peers must remain in sync after the seek; positions {} vs {} (diff {diff} ms)",
        a.pos_ms,
        b.pos_ms
    );
    assert!(
        a.pos_ms >= seek_target,
        "the user's forward seek must not be undone; A at {} (target was {seek_target})",
        a.pos_ms
    );
}
