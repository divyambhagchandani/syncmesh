//! End-to-end 2-peer regression test for the seek-loop bug.
//!
//! Wires two real `RoomState`s with their own `EchoGuard`s in-memory and
//! drives them through the exact scenario that caused the ping-pong on
//! Windows: peer A clicks ahead, peer B's mpv snaps to a different keyframe.
//!
//! Higher fidelity than `seek_loop_prevention.rs`, which only tests one
//! direction (B's echo guard contract). This test proves the loop doesn't
//! happen by:
//!
//!   1. Driving A through the *full* "user clicks → broadcast" path,
//!      including arming A's local-origin echo guard from `send_mpv` and
//!      having A's mpv reply post-`PlaybackRestart`.
//!   2. Delivering A's broadcast to B and exercising the same `App` glue
//!      (`classify_post_seek_timepos`) that production uses, so a future
//!      regression in `on_mpv_time_pos` is caught here too.
//!   3. Simulating B's mpv snapping to a keyframe far enough off-target that
//!      pre-fix logic would have re-broadcast as a fresh user action.
//!   4. Asserting B emits **zero** outbound `Frame::Control(Seek)` frames.
//!
//! Pre-fix this test fails at step 4. Post-fix it passes.
//!
//! No real iroh, no real mpv, no async runtime: deterministic, fast, and
//! exercises the full seam that was broken.

use syncmesh::app::{MpvDispatchOrigin, arm_echo_for_dispatch, classify_post_seek_timepos};
use syncmesh::echo::EchoGuard;
use syncmesh_core::{ControlAction, ControlEvent, Frame, Input, NodeId, Output, RoomState};

/// One peer's worth of state — `App` minus the I/O.
struct Peer {
    state: RoomState,
    echo: EchoGuard,
    /// Outbound frames captured from `Output::Broadcast` and
    /// `Output::SendTo`. Cleared between assertions.
    outbox: Vec<Frame>,
}

impl Peer {
    fn new(me: NodeId, name: &str) -> Self {
        Self {
            state: RoomState::new(me, name.into()),
            echo: EchoGuard::new(),
            outbox: Vec::new(),
        }
    }

    fn add_peer(&mut self, peer: NodeId, nick: &str) {
        let outs = self.state.apply(Input::PeerConnected {
            node: peer,
            nickname: nick.into(),
        });
        self.collect(outs, MpvDispatchOrigin::Local);
    }

    /// Apply an Input, route `Output::Mpv` through the same echo-guard arming
    /// that `App::send_mpv` does, and capture broadcasts to the outbox.
    fn step(&mut self, input: Input, origin: MpvDispatchOrigin, now_ms: u64) {
        let outs = self.state.apply(input);
        self.collect_at(outs, origin, now_ms);
    }

    fn collect(&mut self, outs: Vec<Output>, origin: MpvDispatchOrigin) {
        // Most call sites don't need a custom `now_ms` — use 0 since the
        // echo guard's deadlines are relative.
        self.collect_at(outs, origin, 0);
    }

    fn collect_at(&mut self, outs: Vec<Output>, origin: MpvDispatchOrigin, now_ms: u64) {
        for out in outs {
            match out {
                Output::Broadcast(f) | Output::SendTo { frame: f, .. } => self.outbox.push(f),
                Output::Mpv(cmd) => {
                    arm_echo_for_dispatch(&mut self.echo, cmd, origin, now_ms);
                }
                Output::Notify(_) => {}
            }
        }
    }

    fn drain_seek_broadcasts(&mut self) -> Vec<u64> {
        let seeks = self
            .outbox
            .iter()
            .filter_map(|f| match f {
                Frame::Control(ev) => match ev.action {
                    ControlAction::Seek { media_pos_ms } => Some(media_pos_ms),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        self.outbox.clear();
        seeks
    }
}

fn node(b: u8) -> NodeId {
    NodeId::from_bytes([b; 32])
}

/// Deliver a frame from `from`'s outbox to `to`'s state machine, marking the
/// dispatch as remote-origin (this is what `App::on_peer_frame` does).
fn deliver(to: &mut Peer, from: NodeId, frame: Frame, now_ms: u64) {
    to.step(
        Input::FrameReceived {
            from,
            frame,
            received_at_ms: now_ms,
            rtt_sample_ms: None,
        },
        MpvDispatchOrigin::Remote,
        now_ms,
    );
}

#[test]
fn user_click_on_a_does_not_cause_b_to_re_broadcast_when_mpv_keyframe_drifts() {
    let a_id = node(1);
    let b_id = node(2);

    let mut a = Peer::new(a_id, "alice");
    let mut b = Peer::new(b_id, "bob");
    a.add_peer(b_id, "bob");
    b.add_peer(a_id, "alice");
    // PeerConnected emits an auto-seek-new-joiner frame (plan decision 12).
    // Drop those setup frames so the assertions below only see what the seek
    // scenario itself produces.
    a.outbox.clear();
    b.outbox.clear();

    // Step 1: user on A clicks 10 minutes ahead. mpv reports the click via
    // Seeking → PlaybackRestart → TimePos. App's `on_mpv_time_pos` calls
    // `classify_post_seek_timepos`, which (no local-origin entry pending,
    // because A's user *initiated* the seek) treats it as LocalControl.
    let click_ms = 600_000;
    let user_input_a = classify_post_seek_timepos(&mut a.echo, click_ms, false, 100, 1_000);
    a.step(user_input_a, MpvDispatchOrigin::Local, 1_000);

    // A must have broadcast exactly one Seek frame.
    let a_seeks = a.drain_seek_broadcasts();
    assert_eq!(
        a_seeks,
        vec![click_ms],
        "A must broadcast its user-click seek to the mesh",
    );

    // Find the broadcast frame and deliver it to B.
    let broadcast = Frame::Control(ControlEvent {
        origin: a_id,
        origin_ts_ms: 1_000,
        seq: 1,
        action: ControlAction::Seek {
            media_pos_ms: click_ms,
        },
    });
    deliver(&mut b, a_id, broadcast, 1_050);

    // Step 2: B's state machine emitted Output::Mpv(Seek), which armed B's
    // echo guard as remote-origin via collect_at(). B must NOT have
    // broadcast anything yet — the mpv echo hasn't arrived.
    assert!(
        b.drain_seek_broadcasts().is_empty(),
        "B must not broadcast on the inbound frame itself",
    );

    // Step 3: B's mpv snaps to a keyframe 1500 ms short of the requested
    // target (matches the failure mode that the 1-second SEEK_TOLERANCE_MS
    // would have rejected pre-fix on Windows).
    let b_mpv_landed_at = click_ms - 1_500;
    let post_seek_input =
        classify_post_seek_timepos(&mut b.echo, b_mpv_landed_at, false, 100, 1_200);

    // Pre-fix: this is `Input::LocalControl(Seek)` and B re-broadcasts → loop.
    // Post-fix: it must be `Input::MpvStateUpdate` — the remote-origin echo
    // guard consumed the post-PlaybackRestart TimePos unconditionally.
    match &post_seek_input {
        Input::MpvStateUpdate { media_pos_ms, .. } => {
            assert_eq!(*media_pos_ms, b_mpv_landed_at);
        }
        Input::LocalControl { action, .. } => {
            panic!(
                "regression: B treated its own remote-driven seek echo as a user action: {action:?} \
                 — this is the exact path that caused the 2-peer ping-pong loop on Windows",
            );
        }
        other => panic!("unexpected classification: {other:?}"),
    }

    b.step(post_seek_input, MpvDispatchOrigin::Local, 1_200);

    // Step 4: the assertion. B must NOT have emitted a Seek broadcast.
    let b_seeks_after_mpv_echo = b.drain_seek_broadcasts();
    assert!(
        b_seeks_after_mpv_echo.is_empty(),
        "B re-broadcast {} seek(s) in response to mpv's keyframe-snapped reply: {b_seeks_after_mpv_echo:?} \
         — this is the seek-loop bug",
        b_seeks_after_mpv_echo.len(),
    );
}

#[test]
fn local_user_seek_still_broadcasts_normally() {
    // Negative control: if a user actually keys a seek with no remote-origin
    // entry pending, the broadcast path must still fire. Catches an
    // over-aggressive fix that suppresses everything.
    let me_id = node(1);
    let other_id = node(2);

    let mut me = Peer::new(me_id, "me");
    me.add_peer(other_id, "other");
    me.outbox.clear();

    let click_ms = 30_000;
    let input = classify_post_seek_timepos(&mut me.echo, click_ms, false, 100, 500);

    matches!(input, Input::LocalControl { .. })
        .then_some(())
        .expect("user-initiated seek with no echo entry must produce LocalControl");

    me.step(input, MpvDispatchOrigin::Local, 500);
    assert_eq!(me.drain_seek_broadcasts(), vec![click_ms]);
}
