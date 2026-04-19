//! Per-peer reader/writer tasks and the accept loop.
//!
//! For each `PeerLink` we own we spawn three tasks:
//!
//! - a **control reader** that loops on `peer.recv_frame()` and forwards each
//!   reliable frame into the main event channel,
//! - a **datagram reader** that does the same for `peer.recv_datagram()`,
//! - a **writer** consuming a per-peer `mpsc` of frames to send reliably.
//!
//! Datagrams (heartbeats) are sent synchronously from the main loop via the
//! `PeerLink::send_datagram` method — it doesn't block, so there's no need for
//! a writer task for the unreliable path.

use syncmesh_core::Frame;
use syncmesh_net::{MeshEndpoint, PeerLink};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::LoopEvent;

/// How many outbound frames we'll queue per peer before backpressure kicks in.
/// Control stream is reliable and ordered; if we're this far behind, something
/// upstream is already wedged and dropping is the lesser evil.
const WRITER_QUEUE: usize = 256;

pub fn spawn_peer_tasks(link: &PeerLink, events: &mpsc::Sender<LoopEvent>) -> mpsc::Sender<Frame> {
    let node = link.remote();

    // Writer channel + task
    let (write_tx, mut write_rx) = mpsc::channel::<Frame>(WRITER_QUEUE);
    {
        let link = link.clone();
        tokio::spawn(async move {
            while let Some(frame) = write_rx.recv().await {
                if let Err(e) = link.send_frame(&frame).await {
                    warn!(peer = ?node, error = %e, "send_frame failed; closing peer");
                    break;
                }
            }
            debug!(peer = ?node, "writer task exiting");
        });
    }

    // Control reader
    {
        let link = link.clone();
        let events = events.clone();
        tokio::spawn(async move {
            loop {
                match link.recv_frame().await {
                    Ok(frame) => {
                        let ev = LoopEvent::Peer { from: node, frame };
                        if events.send(ev).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(peer = ?node, error = %e, "control stream closed");
                        let _ = events.send(LoopEvent::PeerDisconnected { node }).await;
                        break;
                    }
                }
            }
        });
    }

    // Datagram reader
    {
        let link = link.clone();
        let events = events.clone();
        tokio::spawn(async move {
            loop {
                match link.recv_datagram().await {
                    Ok(frame) => {
                        let ev = LoopEvent::Peer { from: node, frame };
                        if events.send(ev).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!(peer = ?node, error = %e, "datagram channel closed");
                        break;
                    }
                }
            }
        });
    }

    write_tx
}

pub fn spawn_accept_task(mesh: MeshEndpoint, events: mpsc::Sender<LoopEvent>) {
    tokio::spawn(async move {
        loop {
            match mesh.accept_next().await {
                Ok(Some(link)) => {
                    let remote = link.remote();
                    info!(peer = ?remote, "accepted peer");
                    if events
                        .send(LoopEvent::PeerConnected {
                            link,
                            addr_bytes: None,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    debug!("mesh endpoint closed; accept loop exiting");
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "accept_next failed; continuing");
                }
            }
        }
    });
}
