//! 32-byte peer identifier (Ed25519 public key).
//!
//! The core crate does not depend on iroh, so `NodeId` is defined locally. The
//! net crate builds these from `iroh::NodeId::as_bytes()`; the equality and
//! ordering semantics used by the sync state machine depend only on the 32-byte
//! representation.

use core::fmt;

use serde::{Deserialize, Serialize};

/// A 32-byte Ed25519 public key identifying a peer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// Returns the raw 32 bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Constructs a `NodeId` from a byte array.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<[u8; 32]> for NodeId {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // First 8 hex chars is enough to disambiguate in logs; full bytes can be
        // printed via `{:?}` on `.as_bytes()` when needed.
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        f.write_str("…")
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_lexicographic_over_bytes() {
        let a = NodeId::from_bytes([0u8; 32]);
        let mut b_bytes = [0u8; 32];
        b_bytes[0] = 1;
        let b = NodeId::from_bytes(b_bytes);
        assert!(a < b);
    }

    #[test]
    fn debug_truncates_to_four_bytes() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xde;
        bytes[1] = 0xad;
        bytes[2] = 0xbe;
        bytes[3] = 0xef;
        let id = NodeId::from_bytes(bytes);
        assert_eq!(format!("{id:?}"), "deadbeef…");
    }

    #[test]
    fn display_prints_all_bytes() {
        let id = NodeId::from_bytes([0xab; 32]);
        let s = format!("{id}");
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c == 'a' || c == 'b'));
    }

    #[test]
    fn as_bytes_round_trips() {
        let bytes = [42u8; 32];
        let id = NodeId::from_bytes(bytes);
        assert_eq!(id.as_bytes(), &bytes);
    }
}
