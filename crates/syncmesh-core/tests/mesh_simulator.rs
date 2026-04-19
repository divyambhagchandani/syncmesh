//! Multi-peer mesh simulator.
//!
//! Builds a small cluster of `RoomState`s, routes their outputs between each
//! other with optional fixed propagation delay, and asserts end-to-end
//! properties.
//!
//! Kept in `tests/` so it exercises only the public API of `syncmesh-core` —
//! if these tests break, the public contract broke.

use std::collections::{BTreeMap, VecDeque};

use syncmesh_core::{ControlAction, Frame, Input, MpvCommand, NodeId, Output, RoomState};

/// An in-flight frame waiting to be delivered.
struct Pending {
    deliver_at: u64,
    from: NodeId,
    to: NodeId,
    frame: Frame,
    rtt_sample_ms: Option<u32>,
}

/// A deterministic in-process mesh.
struct Mesh {
    nodes: BTreeMap<NodeId, RoomState>,
    /// Minimum propagation delay, in simulated ms, applied to every outbound
    /// frame. 0 = instantaneous delivery.
    link_delay_ms: u64,
    queue: VecDeque<Pending>,
    now_ms: u64,
}

impl Mesh {
    fn new(ids: &[NodeId], link_delay_ms: u64) -> Self {
        let mut nodes = BTreeMap::new();
        for id in ids {
            nodes.insert(*id, RoomState::new(*id, format!("{id:?}")));
        }
        // Register every node as a peer of every other node. In production
        // the net layer would do this on connection; here we short-circuit.
        for id in ids {
            for other in ids {
                if id == other {
                    continue;
                }
                if let Some(room) = nodes.get_mut(id) {
                    room.apply(Input::PeerConnected {
                        node: *other,
                        nickname: format!("{other:?}"),
                    });
                }
            }
        }
        Self {
            nodes,
            link_delay_ms,
            queue: VecDeque::new(),
            now_ms: 0,
        }
    }

    fn dispatch(&mut self, emitter: NodeId, outputs: Vec<Output>) {
        for out in outputs {
            match out {
                Output::Broadcast(frame) => {
                    for peer in self.nodes.keys().copied().collect::<Vec<_>>() {
                        if peer == emitter {
                            continue;
                        }
                        self.queue.push_back(Pending {
                            deliver_at: self.now_ms + self.link_delay_ms,
                            from: emitter,
                            to: peer,
                            frame: frame.clone(),
                            rtt_sample_ms: Some(u32::try_from(self.link_delay_ms * 2).unwrap_or(0)),
                        });
                    }
                }
                Output::SendTo { to, frame } => {
                    self.queue.push_back(Pending {
                        deliver_at: self.now_ms + self.link_delay_ms,
                        from: emitter,
                        to,
                        frame,
                        rtt_sample_ms: Some(u32::try_from(self.link_delay_ms * 2).unwrap_or(0)),
                    });
                }
                // Mpv commands and notices are local effects — the harness
                // inspects them via `advance_and_collect_mpv`, not here.
                Output::Mpv(_) | Output::Notify(_) => {}
            }
        }
    }

    /// Drain all pending frames whose delivery time is `<= now_ms` and feed
    /// them into the destination rooms. Collects any resulting mpv commands
    /// per node.
    fn flush(&mut self) -> Vec<(NodeId, MpvCommand)> {
        let mut applied_mpv = Vec::new();
        loop {
            // Find any frame ready to deliver.
            let next_idx = self.queue.iter().position(|p| p.deliver_at <= self.now_ms);
            let Some(idx) = next_idx else { break };
            let pending = self.queue.remove(idx).expect("index came from position()");
            let received_at = self.now_ms;
            let outputs = self
                .nodes
                .get_mut(&pending.to)
                .expect("destination exists")
                .apply(Input::FrameReceived {
                    from: pending.from,
                    frame: pending.frame,
                    received_at_ms: received_at,
                    rtt_sample_ms: pending.rtt_sample_ms,
                });
            for out in &outputs {
                if let Output::Mpv(cmd) = out {
                    applied_mpv.push((pending.to, *cmd));
                }
            }
            self.dispatch(pending.to, outputs);
        }
        applied_mpv
    }

    fn tick_all(&mut self) -> Vec<(NodeId, MpvCommand)> {
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        let mut applied = Vec::new();
        for id in ids {
            let outs = self
                .nodes
                .get_mut(&id)
                .expect("node exists")
                .apply(Input::Tick {
                    now_ms: self.now_ms,
                });
            for out in &outs {
                if let Output::Mpv(cmd) = out {
                    applied.push((id, *cmd));
                }
            }
            self.dispatch(id, outs);
        }
        applied
    }

    fn advance_to(&mut self, now_ms: u64) -> Vec<(NodeId, MpvCommand)> {
        assert!(now_ms >= self.now_ms, "time must move forward");
        self.now_ms = now_ms;
        self.flush()
    }

    fn local_control(&mut self, node: NodeId, action: ControlAction) -> Vec<(NodeId, MpvCommand)> {
        let outs = self
            .nodes
            .get_mut(&node)
            .expect("node exists")
            .apply(Input::LocalControl {
                action,
                now_ms: self.now_ms,
            });
        let mut applied: Vec<(NodeId, MpvCommand)> = outs
            .iter()
            .filter_map(|o| match o {
                Output::Mpv(cmd) => Some((node, *cmd)),
                _ => None,
            })
            .collect();
        self.dispatch(node, outs);
        applied.extend(self.flush());
        applied
    }

    fn local_ready(&mut self, node: NodeId, ready: bool) {
        let outs = self
            .nodes
            .get_mut(&node)
            .expect("node exists")
            .apply(Input::LocalReady {
                ready,
                now_ms: self.now_ms,
            });
        self.dispatch(node, outs);
        let _ = self.flush();
    }

    fn mpv_state(&mut self, node: NodeId, pos_ms: u64, paused: bool) {
        let outs = self
            .nodes
            .get_mut(&node)
            .expect("node exists")
            .apply(Input::MpvStateUpdate {
                media_pos_ms: pos_ms,
                paused,
                speed_centi: 100,
            });
        // Passive state updates have no outputs in the current state machine,
        // but guard in case that changes.
        self.dispatch(node, outs);
    }
}

fn n(b: u8) -> NodeId {
    NodeId::from_bytes([b; 32])
}

#[test]
fn three_peer_pause_propagates_to_everyone() {
    let ids = [n(1), n(2), n(3)];
    let mut mesh = Mesh::new(&ids, 10); // 10 ms link delay
    mesh.advance_to(100);

    // All three peers start at pos=0, playing.
    for id in ids {
        mesh.mpv_state(id, 0, false);
    }

    // Peer 1 pauses at pos=1000.
    let applied = mesh.local_control(
        n(1),
        ControlAction::Pause {
            media_pos_ms: 1_000,
        },
    );

    // Advance past the link delay so the broadcast actually arrives. Capture
    // the mpv commands produced by peers 2 and 3 on receipt.
    let after_deliver = mesh.advance_to(200);
    let combined: Vec<_> = applied.into_iter().chain(after_deliver).collect();

    for id in [n(2), n(3)] {
        assert!(
            combined
                .iter()
                .any(|(n_, c)| *n_ == id && matches!(c, MpvCommand::Pause(true))),
            "peer {id:?} did not receive Pause(true). Got: {combined:?}"
        );
        assert!(
            combined.iter().any(|(n_, c)| *n_ == id
                && matches!(
                    c,
                    MpvCommand::Seek {
                        media_pos_ms: 1_000
                    }
                )),
            "peer {id:?} did not receive Seek(1000). Got: {combined:?}"
        );
    }
}

#[test]
fn heartbeat_delivery_lets_a_lagging_peer_catch_up_via_drift() {
    let ids = [n(1), n(2)];
    let mut mesh = Mesh::new(&ids, 5);
    mesh.advance_to(1_000);

    // Both are playing. Peer 1 is at pos=10_000, peer 2 is at pos=9_500 — i.e.
    // peer 2 is 500 ms behind. Drift runs on Tick; we need a first round of
    // heartbeat emission + delivery, then a second tick for the receiver to
    // observe the drift.
    mesh.mpv_state(n(1), 10_000, false);
    mesh.mpv_state(n(2), 9_500, false);

    // Round 1: both emit heartbeats.
    let _ = mesh.tick_all();
    // Advance past link delay so the heartbeats actually deliver.
    let _ = mesh.advance_to(1_010);
    // Round 2: peer 2 now has peer 1's heartbeat and can compute drift.
    let tick_mpv = mesh.tick_all();

    // Peer 2 should detect the lag and emit a SetSpeed with centi > 100.
    let speedups: Vec<_> = tick_mpv
        .iter()
        .filter(|(id, cmd)| {
            *id == n(2) && matches!(cmd, MpvCommand::SetSpeed { speed_centi } if *speed_centi > 100)
        })
        .collect();
    assert!(
        !speedups.is_empty(),
        "peer 2 did not speed up in response to peer 1's lead. All mpv: {tick_mpv:?}"
    );
}

#[test]
fn ready_gate_converges_across_mesh_after_unanimous_ready() {
    let ids = [n(1), n(2), n(3)];
    let mut mesh = Mesh::new(&ids, 5);
    mesh.advance_to(0);

    // No one ready yet — everyone Pending.
    for id in ids {
        let r = mesh.nodes.get(&id).unwrap();
        assert_eq!(
            r.ready_state(),
            syncmesh_core::ReadyState::Pending,
            "{id:?} should be Pending before anyone is ready"
        );
    }

    // Each marks ready. After each, flush to propagate the Presence event.
    for id in ids {
        mesh.local_ready(id, true);
        mesh.advance_to(mesh.now_ms + 10);
    }

    // But presence-Ready alone is not enough — the mesh also relies on heartbeats
    // to advertise ready. Run one round of ticks + flush.
    mesh.advance_to(mesh.now_ms + 100);
    mesh.tick_all();
    mesh.advance_to(mesh.now_ms + 100);

    for id in ids {
        let r = mesh.nodes.get(&id).unwrap();
        assert_eq!(
            r.ready_state(),
            syncmesh_core::ReadyState::AllReady,
            "{id:?} should be AllReady after all peers signalled ready"
        );
    }
}

#[test]
fn simultaneous_pause_from_two_peers_converges_deterministically() {
    // Plan §2.2: the lower (origin_ts, origin_id) wins. Both peers emit Pause
    // near-simultaneously; every peer must see the *same* canonical pause
    // position despite having applied events in different orders.
    let ids = [n(1), n(2), n(3)];
    let mut mesh = Mesh::new(&ids, 5);
    mesh.advance_to(1_000);

    for id in ids {
        mesh.mpv_state(id, 5_000, false);
    }

    // Both peers pause at the same wall-clock ms. We emit both events and
    // then dispatch them without flushing in between, so each peer receives
    // the other's after it has already recorded its own as `last_applied`.
    // n(1) has the smaller NodeId → should win under the tuple tiebreaker.
    let outs1 = mesh
        .nodes
        .get_mut(&n(1))
        .unwrap()
        .apply(Input::LocalControl {
            action: ControlAction::Pause {
                media_pos_ms: 5_000,
            },
            now_ms: 1_000,
        });
    let outs2 = mesh
        .nodes
        .get_mut(&n(2))
        .unwrap()
        .apply(Input::LocalControl {
            action: ControlAction::Pause {
                media_pos_ms: 5_100,
            },
            now_ms: 1_000,
        });
    mesh.dispatch(n(1), outs1);
    mesh.dispatch(n(2), outs2);

    mesh.advance_to(2_000);
    mesh.flush();

    // Peer 3, having seen both, should have its local_playback at pos=5_000
    // (the canonical lower-tuple winner from n(1)).
    let p3 = mesh.nodes.get(&n(3)).unwrap();
    assert!(p3.local_playback().paused);
    assert_eq!(
        p3.local_playback().media_pos_ms,
        5_000,
        "peer 3 should converge to the canonical pause position"
    );
}
