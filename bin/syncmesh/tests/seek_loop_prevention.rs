//! Regression test for the 2-peer seek-echo ping-pong on Windows.
//!
//! Scenario reproduced:
//!   - Peer A clicks far ahead in mpv. A broadcasts `Seek { ms_A }`.
//!   - Peer B's `RoomState` applies the inbound control event and emits
//!     `Output::Mpv(Seek { ms_A })`.
//!   - The bin's `send_mpv` arms the echo guard for that command.
//!   - Mpv on B snaps to a different keyframe (or named-pipe IPC delays the
//!     `PlaybackRestart` reply), so the post-seek `TimePos` lands far enough
//!     from `ms_A` that the local-origin keyframe-snap tolerance would miss.
//!
//! Pre-fix behaviour: B's echo guard fails to consume, B re-broadcasts a
//! fresh `Seek { ms_B }` to A, A snaps differently, re-broadcasts to B, etc.
//! — both peers oscillate between two stable keyframes forever.
//!
//! Post-fix behaviour: B tags the pending entry as remote-origin via
//! `record_seek_remote`, which consumes any in-window `TimePos` regardless
//! of position drift. No re-broadcast, no loop.
//!
//! This test wires a `RoomState` to an `EchoGuard` exactly the way `App` does
//! after the fix and asserts the contract holds end-to-end. It deliberately
//! does not spin up the full `App` event loop or networking: the failure mode
//! is in the `RoomState → send_mpv → EchoGuard` seam, not in the transport.

use syncmesh::echo::EchoGuard;
use syncmesh_core::{
    ControlAction, ControlEvent, Frame, Input, MpvCommand, NodeId, Output, RoomState,
};

fn node(b: u8) -> NodeId {
    NodeId::from_bytes([b; 32])
}

#[test]
fn remote_driven_seek_does_not_re_broadcast_when_mpv_snaps_far_off() {
    // B is the local peer running this RoomState; A is the remote peer that
    // initiated the seek.
    let me = node(2);
    let a = node(1);

    let mut state = RoomState::new(me, "bob".into());
    let _ = state.apply(Input::PeerConnected {
        node: a,
        nickname: "alice".into(),
    });

    // A clicks ten minutes ahead and broadcasts. Peer B receives the frame.
    let seek_ms_a = 600_000;
    let outs = state.apply(Input::FrameReceived {
        from: a,
        frame: Frame::Control(ControlEvent {
            origin: a,
            origin_ts_ms: 1_000,
            seq: 1,
            action: ControlAction::Seek {
                media_pos_ms: seek_ms_a,
            },
        }),
        received_at_ms: 1_010,
        rtt_sample_ms: None,
    });

    // The state machine must emit an MpvCommand::Seek to the local mpv.
    let mpv_cmd = outs.iter().find_map(|o| match o {
        Output::Mpv(cmd) => Some(*cmd),
        _ => None,
    });
    assert_eq!(
        mpv_cmd,
        Some(MpvCommand::Seek {
            media_pos_ms: seek_ms_a
        }),
        "inbound Seek must produce a matching mpv Seek command",
    );

    // Mirror what `App::send_mpv` does in the post-fix code: because this
    // dispatch came from `dispatch_remote` (the on_peer_frame path), the
    // pending entry is tagged remote-origin.
    let mut echo = EchoGuard::new();
    let now = 1_020;
    echo.record_seek_remote(seek_ms_a, now);

    // Mpv on B snaps to its local keyframe — far enough away that the old
    // local-origin `SEEK_TOLERANCE_MS = 1_000` check would fail.
    let mpv_landed_at = seek_ms_a - 1_500;
    let consumed = echo.consume_seek(mpv_landed_at, now + 200);

    // Pre-fix: this asserts false → bin re-broadcasts → ping-pong.
    // Post-fix: remote-origin entries skip tolerance and consume in-window.
    assert!(
        consumed,
        "echo guard must suppress the post-PlaybackRestart TimePos for a \
         remote-driven seek even when mpv's keyframe snap drifts beyond the \
         local-origin tolerance; otherwise the bin re-broadcasts and starts \
         a 2-peer ping-pong loop",
    );
}

#[test]
fn local_user_seek_still_uses_keyframe_snap_tolerance() {
    // Inverse contract: when the local user seeks, an mpv reply far from the
    // requested target is treated as a fresh user action (not an echo). This
    // protects the case where the user keys another seek before mpv has
    // replied to the first.
    let mut echo = EchoGuard::new();
    echo.record_seek(10_000, 0);
    // mpv replied with a position 5 s away — must NOT be treated as our echo.
    assert!(!echo.consume_seek(15_000, 100));
}
