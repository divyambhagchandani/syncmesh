//! Wrapping layer over an `iroh::Endpoint` that speaks the syncmesh ALPN.
//!
//! Responsibilities:
//!
//! - Bind an `Endpoint` with the supplied `SecretKey` and our ALPN.
//! - Expose a ticket string for sharing the room.
//! - Accept incoming connections that carry our ALPN, wrapping each in a
//!   `PeerLink`.
//! - Dial a remote peer by `EndpointAddr` and return a `PeerLink`.
//!
//! Discovery/relay policy is set by `MeshConfig`:
//! - `MeshConfig::default()` uses `presets::N0` — n0 relays + pkarr DNS
//!   address lookup. Appropriate for real-world use.
//! - `MeshConfig::localhost()` uses `presets::Empty` plus the ring crypto
//!   provider, disables relay, and skips address lookup. Used by loopback
//!   integration tests; not suitable for actual NAT traversal.

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey};

use crate::peer::{PeerLink, PeerLinkError};
use crate::ticket;

/// ALPN string identifying the syncmesh control protocol. Any connection
/// presenting a different ALPN is rejected at the TLS layer.
pub const ALPN: &[u8] = b"syncmesh/0";

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("binding iroh endpoint failed: {0}")]
    Bind(String),
    #[error("accepting incoming connection failed: {0}")]
    Accept(String),
    #[error("dialing peer failed: {0}")]
    Connect(String),
    #[error(transparent)]
    PeerLink(#[from] PeerLinkError),
    #[error("the mesh endpoint has been closed")]
    Closed,
}

/// Transport-level configuration for a mesh endpoint.
#[derive(Debug, Clone, Default)]
pub struct MeshConfig {
    /// Use the localhost-friendly preset (no relay, no DNS address lookup).
    /// Always `false` for production use; `true` for integration tests.
    pub localhost_only: bool,
    /// When `Some`, override the default (n0) relay with this custom URL.
    /// Ignored if `localhost_only` is `true` (that preset disables relays
    /// entirely). Per decision 21 we keep this hidden from the UI — it's a
    /// config-file-only knob for users running their own iroh-relay.
    pub relay_override: Option<RelayUrl>,
}

impl MeshConfig {
    pub const fn localhost() -> Self {
        Self {
            localhost_only: true,
            relay_override: None,
        }
    }

    /// Builder-style setter for the self-hosted relay URL.
    #[must_use]
    pub fn with_relay_override(mut self, url: RelayUrl) -> Self {
        self.relay_override = Some(url);
        self
    }
}

/// A bound iroh `Endpoint` configured for the syncmesh protocol.
#[derive(Debug, Clone)]
pub struct MeshEndpoint {
    inner: Endpoint,
}

impl MeshEndpoint {
    /// Bind a new endpoint using the supplied identity.
    pub async fn bind(secret: SecretKey, config: MeshConfig) -> Result<Self, MeshError> {
        let inner = if config.localhost_only {
            Endpoint::builder(presets::Minimal)
                .secret_key(secret)
                .alpns(vec![ALPN.to_vec()])
                .relay_mode(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .map_err(|e| MeshError::Bind(e.to_string()))?
                .bind()
                .await
                .map_err(|e| MeshError::Bind(e.to_string()))?
        } else {
            let mut builder = Endpoint::builder(presets::N0)
                .secret_key(secret)
                .alpns(vec![ALPN.to_vec()]);
            if let Some(relay) = config.relay_override {
                builder = builder.relay_mode(RelayMode::custom([relay]));
            }
            builder
                .bind()
                .await
                .map_err(|e| MeshError::Bind(e.to_string()))?
        };
        Ok(Self { inner })
    }

    /// Return this endpoint's `EndpointAddr` at the current moment in time.
    /// For a localhost-only endpoint this contains the bound IP addresses;
    /// for a real one it will also contain a relay URL once online.
    pub fn addr(&self) -> EndpointAddr {
        self.inner.addr()
    }

    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.inner.id()
    }

    /// Encode the current `addr()` as a shareable ticket string.
    pub fn ticket(&self) -> String {
        ticket::encode(&self.addr())
    }

    /// Reference to the underlying iroh endpoint. Exposed mainly for tests and
    /// for the mesh-coordination layer (Phase 4) that needs to attach further
    /// protocol handlers.
    pub fn raw(&self) -> &Endpoint {
        &self.inner
    }

    /// Dial a remote peer and establish the per-pair control stream.
    ///
    /// The returned `PeerLink` has already opened the bidirectional stream,
    /// so callers can immediately send frames without waiting for a round-trip.
    pub async fn dial(&self, addr: impl Into<EndpointAddr>) -> Result<PeerLink, MeshError> {
        let conn = self
            .inner
            .connect(addr, ALPN)
            .await
            .map_err(|e| MeshError::Connect(e.to_string()))?;
        let link = PeerLink::open(conn).await?;
        Ok(link)
    }

    /// Wait for the next incoming connection and complete its handshake.
    /// Returns `None` once the endpoint is closed.
    ///
    /// Returns a `PeerLink` that has already accepted the dialer's bidi
    /// control stream.
    pub async fn accept_next(&self) -> Result<Option<PeerLink>, MeshError> {
        let Some(incoming) = self.inner.accept().await else {
            return Ok(None);
        };
        let conn = incoming
            .await
            .map_err(|e| MeshError::Accept(e.to_string()))?;
        let link = PeerLink::accept(conn).await?;
        Ok(Some(link))
    }

    /// Gracefully shut the endpoint down. All live connections are closed.
    pub async fn close(&self) {
        self.inner.close().await;
    }
}
