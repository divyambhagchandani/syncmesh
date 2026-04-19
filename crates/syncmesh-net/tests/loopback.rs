//! Two real iroh endpoints in one process, exchanging frames and datagrams.
//!
//! This is the Phase 2 exit-criterion test: "Two endpoints in one process
//! exchange frames over the real iroh stack." Each test binds both endpoints
//! with the `localhost` mesh config (no relay, no DNS address lookup), dials
//! one from the other using the accept-side's `EndpointAddr`, and then
//! exercises the public `PeerLink` surface.
//!
//! These tests run without network access. They use the loopback interface
//! only, which is why `MeshConfig::localhost()` is essential — the default
//! config would attempt to contact n0 relays and a public DNS server.

use std::time::Duration;

use anyhow::Result;
use syncmesh_core::{
    ChatMessage, ControlAction, ControlEvent, Frame, RttEstimator, StateHeartbeat,
};
use syncmesh_net::{MeshConfig, MeshEndpoint, identity};
use tokio::time::timeout;

/// Bind two endpoints and fully establish a `PeerLink` in both directions.
///
/// Returns `(dialer_endpoint, acceptor_endpoint, dialer_link, acceptor_link)`.
async fn connected_pair() -> Result<(
    MeshEndpoint,
    MeshEndpoint,
    syncmesh_net::PeerLink,
    syncmesh_net::PeerLink,
)> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();

    let accept_secret = identity::generate();
    let dial_secret = identity::generate();

    let acceptor = MeshEndpoint::bind(accept_secret, MeshConfig::localhost()).await?;
    let dialer = MeshEndpoint::bind(dial_secret, MeshConfig::localhost()).await?;

    let accept_addr = acceptor.addr();

    let acceptor_clone = acceptor.clone();
    let accept_task = tokio::spawn(async move {
        acceptor_clone
            .accept_next()
            .await
            .map_err(anyhow::Error::from)?
            .ok_or_else(|| anyhow::anyhow!("endpoint closed before accepting"))
    });

    // Give the dialer a bounded amount of time to complete the handshake — on a
    // loopback socket this should happen in well under a second, and any more
    // than that strongly suggests we're broken.
    let dialer_link = timeout(Duration::from_secs(10), dialer.dial(accept_addr)).await??;
    let acceptor_link = timeout(Duration::from_secs(10), accept_task).await???;

    // Sanity-check both sides agree on who they're talking to.
    assert_eq!(
        dialer_link.remote().as_bytes(),
        acceptor.endpoint_id().as_bytes()
    );
    assert_eq!(
        acceptor_link.remote().as_bytes(),
        dialer.endpoint_id().as_bytes()
    );

    Ok((dialer, acceptor, dialer_link, acceptor_link))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_frame_round_trips_over_stream() -> Result<()> {
    let (dialer, acceptor, dialer_link, acceptor_link) = connected_pair().await?;

    let sent = Frame::Control(ControlEvent {
        origin: dialer_link.remote(),
        origin_ts_ms: 1_234,
        seq: 1,
        action: ControlAction::Pause {
            media_pos_ms: 5_000,
        },
    });

    dialer_link.send_frame(&sent).await?;
    let got = timeout(Duration::from_secs(5), acceptor_link.recv_frame()).await??;
    assert_eq!(got, sent);

    dialer.close().await;
    acceptor.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_frames_preserve_order() -> Result<()> {
    let (dialer, acceptor, dialer_link, acceptor_link) = connected_pair().await?;

    let messages: Vec<Frame> = (0..50u32)
        .map(|i| {
            Frame::Chat(ChatMessage {
                origin: dialer_link.remote(),
                origin_ts_ms: u64::from(i),
                text: format!("message-{i}"),
            })
        })
        .collect();

    // Send all frames from the dialer, and concurrently read them from the
    // acceptor. Order must be preserved.
    let sender_messages = messages.clone();
    let sender_link = dialer_link.clone();
    let sender = tokio::spawn(async move {
        for m in &sender_messages {
            sender_link.send_frame(m).await?;
        }
        Ok::<(), syncmesh_net::PeerLinkError>(())
    });

    for expected in &messages {
        let got = timeout(Duration::from_secs(5), acceptor_link.recv_frame()).await??;
        assert_eq!(&got, expected);
    }
    sender.await??;

    dialer.close().await;
    acceptor.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_via_datagram_round_trips() -> Result<()> {
    let (dialer, acceptor, dialer_link, acceptor_link) = connected_pair().await?;

    // Datagrams must be supported on a fresh iroh QUIC connection.
    assert!(dialer_link.max_datagram_size().is_some());
    assert!(acceptor_link.max_datagram_size().is_some());

    let hb = Frame::Heartbeat(StateHeartbeat {
        origin: dialer_link.remote(),
        origin_ts_ms: 42_000,
        media_pos_ms: 12_345,
        paused: false,
        speed_centi: 100,
        ready: true,
        media_id: None,
    });

    // Datagrams can be dropped or reordered; give the test a few attempts.
    let mut delivered = None;
    for _ in 0..10 {
        dialer_link.send_datagram(&hb)?;
        if let Ok(Ok(f)) = timeout(Duration::from_millis(500), acceptor_link.recv_datagram()).await
        {
            delivered = Some(f);
            break;
        }
    }
    assert_eq!(
        delivered.expect("datagram never delivered"),
        hb,
        "datagram payload differed from what was sent"
    );

    dialer.close().await;
    acceptor.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_round_trip_and_dial() -> Result<()> {
    // Verifies that the ticket a real endpoint produces is actually dialable
    // by a second endpoint — i.e. the base32-prefixed string is a fully
    // sufficient invite artifact, end to end.
    let accept_secret = identity::generate();
    let dial_secret = identity::generate();

    let acceptor = MeshEndpoint::bind(accept_secret, MeshConfig::localhost()).await?;
    let dialer = MeshEndpoint::bind(dial_secret, MeshConfig::localhost()).await?;

    let ticket = acceptor.ticket();
    assert!(
        ticket.starts_with("syncmesh1"),
        "unexpected ticket: {ticket}"
    );
    let parsed_addr = syncmesh_net::decode_ticket(&ticket)?;

    let acceptor_clone = acceptor.clone();
    let accept_task = tokio::spawn(async move { acceptor_clone.accept_next().await });

    let dialer_link = timeout(Duration::from_secs(10), dialer.dial(parsed_addr)).await??;
    let acceptor_link = timeout(Duration::from_secs(10), accept_task)
        .await???
        .expect("endpoint closed before a peer connected");

    // Prove the handshake actually completed by sending a frame through it.
    let f = Frame::Chat(ChatMessage {
        origin: dialer_link.remote(),
        origin_ts_ms: 1,
        text: "hello via ticket".into(),
    });
    dialer_link.send_frame(&f).await?;
    let got = timeout(Duration::from_secs(5), acceptor_link.recv_frame()).await??;
    assert_eq!(got, f);

    dialer.close().await;
    acceptor.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtt_estimator_receives_samples_from_transport() -> Result<()> {
    // Exchange a handful of frames (which forces QUIC to exchange real
    // packets), then poll RTT from the transport and feed the EWMA. We don't
    // care about the value — only that it's populated after traffic flows.
    let (dialer, acceptor, dialer_link, acceptor_link) = connected_pair().await?;

    for i in 0..5u64 {
        let f = Frame::Control(ControlEvent {
            origin: dialer_link.remote(),
            origin_ts_ms: i,
            seq: i,
            action: ControlAction::Seek {
                media_pos_ms: i * 1_000,
            },
        });
        dialer_link.send_frame(&f).await?;
        let _ = timeout(Duration::from_secs(5), acceptor_link.recv_frame()).await??;
    }

    // Paths take a moment to settle. Retry sampling for a bounded time.
    let mut est = RttEstimator::new();
    let mut sampled = false;
    for _ in 0..20 {
        if dialer_link.sample_rtt(&mut est).is_some() {
            sampled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sampled, "no RTT sample was available after traffic flowed");
    let ms = est.estimate_ms().expect("EWMA should have a value");
    // Loopback RTT should be comfortably under a second; we allow generous
    // slack because CI runners can be slow.
    assert!(ms < 1_000, "loopback rtt wildly too high: {ms} ms");

    dialer.close().await;
    acceptor.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_disconnect_is_observed_on_the_other_side() -> Result<()> {
    let (dialer, acceptor, dialer_link, acceptor_link) = connected_pair().await?;

    // Acceptor starts reading; dialer closes; acceptor should see a recv
    // error (Eof or transport-level close) rather than hanging forever.
    let reader = tokio::spawn(async move { acceptor_link.recv_frame().await });

    // Let the reader park in its await.
    tokio::time::sleep(Duration::from_millis(50)).await;
    dialer_link.close(0, b"test done");
    dialer.close().await;

    let result = timeout(Duration::from_secs(5), reader).await??;
    assert!(
        result.is_err(),
        "recv_frame unexpectedly returned Ok after remote close"
    );

    acceptor.close().await;
    Ok(())
}
