//! Per-peer link: one control stream + datagram channel + RTT estimator.
//!
//! Each pair of peers in the mesh shares a single QUIC connection. Over that
//! connection we open one bidirectional stream for reliable control events,
//! and use QUIC datagrams for unreliable 1 Hz heartbeats. RTT is pulled from
//! iroh's path telemetry and folded into the core crate's EWMA.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::Watcher;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use syncmesh_core::{Frame, NodeId, RttEstimator};
use tokio::sync::Mutex;

use crate::framing::{FrameRecvError, FrameSendError, read_frame, write_frame};

/// Byte sent by the dialer immediately after opening the control stream.
///
/// QUIC does not announce a freshly-opened bidirectional stream to the peer
/// until the opener actually writes something, so the dialer's `accept_bi()`
/// counterpart blocks until some bytes arrive. A single version byte is the
/// minimum we can send to kick that over, and it doubles as a future-proof
/// protocol-version nibble if we ever need one.
const PROTOCOL_HELLO: u8 = 0x01;

#[derive(Debug, thiserror::Error)]
pub enum PeerLinkError {
    #[error("opening bidi stream failed: {0}")]
    OpenBi(String),
    #[error("accepting bidi stream failed: {0}")]
    AcceptBi(String),
    #[error("sending datagram failed: {0}")]
    Datagram(String),
    #[error(transparent)]
    FrameSend(#[from] FrameSendError),
    #[error(transparent)]
    FrameRecv(#[from] FrameRecvError),
}

/// A live connection to a single remote peer.
///
/// Created either by dialing (`MeshEndpoint::dial`) or by accepting an incoming
/// connection (`MeshEndpoint::accept_next`). Cloning is cheap — the underlying
/// `iroh::endpoint::Connection` is itself cheaply cloneable.
#[derive(Clone)]
pub struct PeerLink {
    remote: NodeId,
    conn: Connection,
    /// Writer half of the control stream, behind a mutex so concurrent senders
    /// can share a `PeerLink` without stepping on each other's length prefixes.
    send: Arc<Mutex<SendStream>>,
    /// Reader half. Reads are serialized through the same pattern: at most one
    /// task should own this at a time.
    recv: Arc<Mutex<RecvStream>>,
}

impl std::fmt::Debug for PeerLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerLink")
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

impl PeerLink {
    /// Construct a `PeerLink` as the dialer: we open a fresh bidi stream that
    /// the peer will `accept_bi()` on the other side.
    pub async fn open(conn: Connection) -> Result<Self, PeerLinkError> {
        let remote = NodeId::from_bytes(*conn.remote_id().as_bytes());
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| PeerLinkError::OpenBi(e.to_string()))?;
        // Flush the hello byte so the acceptor's `accept_bi()` actually fires.
        send.write_all(&[PROTOCOL_HELLO])
            .await
            .map_err(|e| PeerLinkError::OpenBi(e.to_string()))?;
        Ok(Self::from_parts(remote, conn, send, recv))
    }

    /// Construct a `PeerLink` as the acceptor: we wait for the dialer to open
    /// the control stream on its side.
    pub async fn accept(conn: Connection) -> Result<Self, PeerLinkError> {
        let remote = NodeId::from_bytes(*conn.remote_id().as_bytes());
        let (send, mut recv) = conn
            .accept_bi()
            .await
            .map_err(|e| PeerLinkError::AcceptBi(e.to_string()))?;
        let mut hello = [0u8; 1];
        recv.read_exact(&mut hello)
            .await
            .map_err(|e| PeerLinkError::AcceptBi(e.to_string()))?;
        if hello[0] != PROTOCOL_HELLO {
            return Err(PeerLinkError::AcceptBi(format!(
                "unexpected protocol hello byte: 0x{:02x}",
                hello[0]
            )));
        }
        Ok(Self::from_parts(remote, conn, send, recv))
    }

    fn from_parts(remote: NodeId, conn: Connection, send: SendStream, recv: RecvStream) -> Self {
        Self {
            remote,
            conn,
            send: Arc::new(Mutex::new(send)),
            recv: Arc::new(Mutex::new(recv)),
        }
    }

    pub fn remote(&self) -> NodeId {
        self.remote
    }

    /// Maximum outbound datagram size the peer has advertised, or `None` if
    /// datagrams are not supported (should never happen on modern iroh).
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    /// Send a reliable, ordered frame on the control stream.
    pub async fn send_frame(&self, frame: &Frame) -> Result<(), PeerLinkError> {
        let mut guard = self.send.lock().await;
        write_frame(&mut *guard, frame).await?;
        Ok(())
    }

    /// Receive the next reliable frame from the control stream. Returns an
    /// `Eof` error on clean remote close.
    pub async fn recv_frame(&self) -> Result<Frame, PeerLinkError> {
        let mut guard = self.recv.lock().await;
        Ok(read_frame(&mut *guard).await?)
    }

    /// Send a frame as a single unreliable QUIC datagram. Falls back to the
    /// control stream is **not** automatic — callers that care about delivery
    /// must use `send_frame` instead. Heartbeats intentionally tolerate loss.
    pub fn send_datagram(&self, frame: &Frame) -> Result<(), PeerLinkError> {
        let bytes = Bytes::from(frame.encode());
        self.conn
            .send_datagram(bytes)
            .map_err(|e| PeerLinkError::Datagram(e.to_string()))
    }

    /// Await the next datagram and decode it as a `Frame`.
    pub async fn recv_datagram(&self) -> Result<Frame, PeerLinkError> {
        let bytes = self
            .conn
            .read_datagram()
            .await
            .map_err(|e| PeerLinkError::Datagram(e.to_string()))?;
        Ok(Frame::decode(&bytes).map_err(FrameRecvError::Decode)?)
    }

    /// Current round-trip-time as reported by the iroh transport, sampled from
    /// the currently selected path. Returns `None` until the first paths update.
    pub fn current_rtt(&self) -> Option<Duration> {
        let paths = self.conn.paths().peek().clone();
        // Prefer the selected path; fall back to the first one for freshly
        // established connections where no path has been elected yet.
        let path = paths
            .iter()
            .find(|p| p.is_selected())
            .or_else(|| paths.iter().next())?;
        self.conn.rtt(path.id())
    }

    /// Sample the current RTT into the estimator. Returns the millisecond
    /// sample applied, or `None` if no path RTT is available yet.
    pub fn sample_rtt(&self, estimator: &mut RttEstimator) -> Option<u32> {
        let d = self.current_rtt()?;
        let ms =
            u32::try_from(d.as_millis().min(u128::from(u64::from(u32::MAX)))).unwrap_or(u32::MAX);
        estimator.sample(ms);
        Some(ms)
    }

    /// Close the underlying connection with an application error code.
    pub fn close(&self, code: u32, reason: &[u8]) {
        self.conn.close(code.into(), reason);
    }

    /// Wait for the connection to be fully closed.
    pub async fn closed(&self) {
        let _ = self.conn.closed().await;
    }
}
