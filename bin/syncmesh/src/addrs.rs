//! Transport-address registry for the bin layer.
//!
//! The core crate keeps `PresenceEvent::PeerList` entries' address fields as
//! opaque `Vec<u8>` so it stays iroh-free. The bin layer is responsible for
//! encoding and decoding those bytes as `iroh::EndpointAddr` via postcard, and
//! for maintaining a `NodeId -> addr_bytes` map used for two purposes:
//!
//! 1. Rewriting the placeholder bytes in any outgoing `PeerList` with the
//!    current best-known addresses.
//! 2. Dialing peers we have learned about via an inbound `PeerList` but are
//!    not yet connected to — giving us the full N>2 mesh per decision 4/8.

use std::collections::HashMap;

use iroh::EndpointAddr;
use syncmesh_core::NodeId;
use syncmesh_net::{MeshEndpoint, endpoint_id_to_node};

/// A `NodeId`-keyed store of opaque address bytes. The bytes are the postcard
/// encoding of an `iroh::EndpointAddr`; callers who need the structured form
/// call [`decode`].
#[derive(Debug, Default)]
pub struct AddrRegistry {
    known: HashMap<NodeId, Vec<u8>>,
}

impl AddrRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the registry with the local endpoint's current address so the
    /// first `PeerList` we send carries our own entry.
    pub fn with_self(mesh: &MeshEndpoint) -> Self {
        let mut r = Self::new();
        let self_node = endpoint_id_to_node(mesh.endpoint_id());
        if let Some(bytes) = encode_addr(&mesh.addr()) {
            r.insert(self_node, bytes);
        }
        r
    }

    /// Record (or overwrite) the known address bytes for a node.
    pub fn insert(&mut self, node: NodeId, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.known.insert(node, bytes);
    }

    /// Return the stored bytes for a node, if any.
    pub fn get(&self, node: &NodeId) -> Option<&[u8]> {
        self.known.get(node).map(Vec::as_slice)
    }

    /// Refresh the registry's entry for the local node. The `EndpointAddr`
    /// iroh hands back from `Endpoint::addr()` can gain/lose direct-IP entries
    /// during NAT discovery; call this before broadcasting an `AddrAnnounce`.
    pub fn refresh_self(&mut self, mesh: &MeshEndpoint) {
        let self_node = endpoint_id_to_node(mesh.endpoint_id());
        if let Some(bytes) = encode_addr(&mesh.addr()) {
            self.known.insert(self_node, bytes);
        }
    }
}

/// Postcard-encode an `EndpointAddr`. Returns `None` if encoding fails — which
/// should be impossible, but encoding errors here are not worth panicking
/// over; callers treat the missing address as "no address known".
pub fn encode_addr(addr: &EndpointAddr) -> Option<Vec<u8>> {
    postcard::to_allocvec(addr).ok()
}

/// Postcard-decode opaque bytes into an `EndpointAddr`. Empty or malformed
/// inputs return `None`.
pub fn decode_addr(bytes: &[u8]) -> Option<EndpointAddr> {
    if bytes.is_empty() {
        return None;
    }
    postcard::from_bytes(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncmesh_net::identity;

    fn sample_addr() -> EndpointAddr {
        let k = identity::generate();
        EndpointAddr::new(k.public()).with_ip_addr("127.0.0.1:12345".parse().unwrap())
    }

    fn node(b: u8) -> NodeId {
        NodeId::from_bytes([b; 32])
    }

    #[test]
    fn encode_decode_round_trips_endpoint_addr() {
        let orig = sample_addr();
        let bytes = encode_addr(&orig).expect("encode ok");
        let parsed = decode_addr(&bytes).expect("decode ok");
        assert_eq!(parsed.id, orig.id);
        let orig_ips: Vec<_> = orig.ip_addrs().copied().collect();
        let parsed_ips: Vec<_> = parsed.ip_addrs().copied().collect();
        assert_eq!(parsed_ips, orig_ips);
    }

    #[test]
    fn decode_empty_returns_none() {
        assert!(decode_addr(&[]).is_none());
    }

    #[test]
    fn decode_garbage_returns_none() {
        assert!(decode_addr(&[0xff; 4]).is_none());
    }

    #[test]
    fn registry_insert_and_get() {
        let mut r = AddrRegistry::new();
        let bytes = encode_addr(&sample_addr()).unwrap();
        r.insert(node(1), bytes.clone());
        assert_eq!(r.get(&node(1)), Some(bytes.as_slice()));
        assert_eq!(r.get(&node(2)), None);
    }

    #[test]
    fn registry_drops_empty_bytes() {
        let mut r = AddrRegistry::new();
        r.insert(node(7), vec![]);
        assert!(r.get(&node(7)).is_none());
    }

    #[test]
    fn registry_insert_overwrites_previous() {
        let mut r = AddrRegistry::new();
        r.insert(node(3), vec![1, 2, 3]);
        r.insert(node(3), vec![9, 9, 9]);
        assert_eq!(r.get(&node(3)), Some([9, 9, 9].as_slice()));
    }
}
