//! iroh mesh transport for syncmesh.
//!
//! This crate owns the iroh-dependent side of the system: it provides a
//! persistent Ed25519 identity, a ticket format for sharing rooms, a
//! `MeshEndpoint` that accepts and dials QUIC connections, and a `PeerLink`
//! that exposes the per-peer control stream and heartbeat datagram channel.
//!
//! The public surface is intentionally narrow: `syncmesh-core` stays I/O-free,
//! and the binary composes these pieces into the full mesh.

pub mod framing;
pub mod identity;
pub mod mesh;
pub mod peer;
pub mod ticket;

pub use framing::{FrameRecvError, FrameSendError, read_frame, write_frame};
pub use identity::{IdentityError, load_or_create};
pub use mesh::{MeshConfig, MeshEndpoint, MeshError, ALPN};
pub use peer::{PeerLink, PeerLinkError};
pub use ticket::{TicketError, decode as decode_ticket, encode as encode_ticket};

/// Convert an `iroh::EndpointId` into the core crate's `NodeId`.
pub fn endpoint_id_to_node(id: iroh::EndpointId) -> syncmesh_core::NodeId {
    syncmesh_core::NodeId::from_bytes(*id.as_bytes())
}
