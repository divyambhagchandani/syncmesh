//! Three-peer full-mesh integration test (Phase 4 exit criterion).
//!
//! Scenario:
//!   - A binds an endpoint.
//!   - B dials A using A's ticket.
//!   - C dials B using B's ticket.
//!   - B sends C a `PresenceEvent::PeerList` carrying A's node id and
//!     postcard-encoded `EndpointAddr`. This is what the real App emits
//!     after a new peer connects (with bytes filled in from its
//!     `AddrRegistry`).
//!   - C decodes the `PeerList`, dials A, establishing a direct link.
//!   - A control frame originated at A over the new A↔C link reaches C.
//!
//! This proves the wire-level plumbing added in Phase 4: the 3-tuple
//! `PeerList` shape, postcard round-trip of `EndpointAddr`, and the
//! dial-unknown-peer path used by the App. It deliberately does *not* spin
//! up full `App` instances — running three mpv-less event loops would
//! balloon the test while testing no additional logic beyond what is
//! covered here plus what is already covered by the net-layer loopback
//! suite.

use std::time::Duration;

use anyhow::Result;
use syncmesh::addrs::{AddrRegistry, decode_addr, encode_addr};
use syncmesh_core::{ControlAction, ControlEvent, Frame, NodeId, PresenceEvent};
use syncmesh_net::{MeshConfig, MeshEndpoint, identity};
use tokio::time::timeout;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
}

async fn bind_localhost() -> Result<MeshEndpoint> {
    Ok(MeshEndpoint::bind(identity::generate(), MeshConfig::localhost()).await?)
}

fn node_of(mesh: &MeshEndpoint) -> NodeId {
    NodeId::from_bytes(*mesh.endpoint_id().as_bytes())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transitive_peer_list_dial_establishes_direct_a_c_link() -> Result<()> {
    init_tracing();

    // Bind A, B, C.
    let a = bind_localhost().await?;
    let b = bind_localhost().await?;
    let c = bind_localhost().await?;

    let a_node = node_of(&a);
    let c_node = node_of(&c);

    // B dials A. Representative of the "join via host ticket" path.
    let a_addr = a.addr();
    let a_clone = a.clone();
    let a_accept_b = tokio::spawn(async move {
        a_clone
            .accept_next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("a closed"))
    });
    let b_to_a = timeout(Duration::from_secs(10), b.dial(a_addr.clone())).await??;
    let _a_from_b = timeout(Duration::from_secs(10), a_accept_b).await???;

    // C dials B. Representative of the "join via second-hop ticket" path.
    let b_addr = b.addr();
    let b_clone = b.clone();
    let b_accept_c = tokio::spawn(async move {
        b_clone
            .accept_next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("b closed"))
    });
    let c_to_b = timeout(Duration::from_secs(10), c.dial(b_addr)).await??;
    let b_from_c = timeout(Duration::from_secs(10), b_accept_c).await???;

    // Build B's outbound PeerList to C by running the same logic the App
    // uses: core emits the frame with empty addr_bytes; the bin fills from
    // its registry. Here we drive that by hand.
    let mut b_registry = AddrRegistry::with_self(&b);
    b_registry.insert(a_node, encode_addr(&a_addr).expect("encode a"));

    let raw_frame = Frame::Presence(PresenceEvent::PeerList {
        peers: vec![(a_node, "alice".into(), Vec::new())],
    });
    let filled = fill_peer_list_with(&b_registry, raw_frame);

    // B → C PeerList. B's writer side is `b_from_c`.
    b_from_c.send_frame(&filled).await?;
    let received = timeout(Duration::from_secs(5), c_to_b.recv_frame()).await??;

    // Verify C can extract A's address from the PeerList and dial A directly.
    let Frame::Presence(PresenceEvent::PeerList { peers }) = received else {
        panic!("unexpected frame shape: {received:?}");
    };
    assert_eq!(peers.len(), 1);
    let (got_node, got_nick, got_bytes) = &peers[0];
    assert_eq!(*got_node, a_node);
    assert_eq!(got_nick, "alice");
    let decoded = decode_addr(got_bytes).expect("addr bytes must decode");
    assert_eq!(decoded.id.as_bytes(), a_addr.id.as_bytes());

    // Have A's acceptor ready before C dials.
    let a_clone2 = a.clone();
    let a_accept_c = tokio::spawn(async move {
        a_clone2
            .accept_next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("a closed"))
    });

    // C dials A using the decoded address — this is the core "N>2 direct
    // link" behavior the Phase 4 change enables.
    let c_to_a = timeout(Duration::from_secs(10), c.dial(decoded)).await??;
    let a_from_c = timeout(Duration::from_secs(10), a_accept_c).await???;

    // Both sides must agree on who they're talking to.
    assert_eq!(c_to_a.remote(), a_node);
    assert_eq!(a_from_c.remote(), c_node);

    // Send a ControlEvent A → C over the freshly established direct link.
    // This proves the link isn't just established but is actually usable:
    // the decision-8 invariant ("host becomes optional at runtime") is
    // satisfied because A and C communicate without transiting B.
    let ev = Frame::Control(ControlEvent {
        origin: a_node,
        origin_ts_ms: 7_777,
        seq: 1,
        action: ControlAction::Pause { media_pos_ms: 42 },
    });
    a_from_c.send_frame(&ev).await?;
    let got = timeout(Duration::from_secs(5), c_to_a.recv_frame()).await??;
    assert_eq!(got, ev);

    // Clean shutdown so the test doesn't leak background tasks.
    drop((b_to_a, c_to_b, c_to_a, a_from_c));
    a.close().await;
    b.close().await;
    c.close().await;
    Ok(())
}

/// Mirror of `App::fill_peer_list_addrs`, inlined here so the integration
/// test doesn't reach into private internals.
fn fill_peer_list_with(registry: &AddrRegistry, frame: Frame) -> Frame {
    let Frame::Presence(PresenceEvent::PeerList { peers }) = frame else {
        return frame;
    };
    let rewritten = peers
        .into_iter()
        .map(|(node, nick, existing)| {
            if !existing.is_empty() {
                return (node, nick, existing);
            }
            let bytes = registry.get(&node).map(<[u8]>::to_vec).unwrap_or_default();
            (node, nick, bytes)
        })
        .collect();
    Frame::Presence(PresenceEvent::PeerList { peers: rewritten })
}
