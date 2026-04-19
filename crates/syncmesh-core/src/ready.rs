//! Ready-state gate.
//!
//! Per plan decisions 12 and 13, every peer advertises a `ready: bool`.
//! Playback can only advance when *all* peers are ready, unless the
//! `override_enabled` flag is set, in which case any peer can force advance.
//! A late joiner is auto-seeked to the current room position but stays paused
//! until it flips its own ready flag — giving it agency without making others
//! wait pointlessly.

use std::collections::BTreeMap;

use crate::node_id::NodeId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyState {
    /// At least one known peer (including self) is not ready. Playback
    /// should remain paused.
    Pending,
    /// Every known peer is ready. Playback may proceed.
    AllReady,
}

/// Tracks per-peer readiness and derives the room-wide `ReadyState`.
///
/// Cheap to clone — holds a `BTreeMap<NodeId, bool>`.
#[derive(Debug, Clone, Default)]
pub struct ReadyGate {
    peers: BTreeMap<NodeId, bool>,
    override_enabled: bool,
}

impl ReadyGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable/disable the "override" escape hatch. When enabled, `state()`
    /// returns `AllReady` as long as at least one peer is ready.
    pub fn set_override(&mut self, enabled: bool) {
        self.override_enabled = enabled;
    }

    pub const fn is_override_enabled(&self) -> bool {
        self.override_enabled
    }

    /// Register or update a peer's readiness. `self` should be included with
    /// the local node id so the gate can reason about the full room.
    pub fn set(&mut self, node: NodeId, ready: bool) {
        self.peers.insert(node, ready);
    }

    /// Remove a peer (e.g. on disconnect).
    pub fn remove(&mut self, node: &NodeId) {
        self.peers.remove(node);
    }

    /// Look up a specific peer's current readiness.
    pub fn get(&self, node: &NodeId) -> Option<bool> {
        self.peers.get(node).copied()
    }

    /// Number of peers tracked (including self).
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Derived room-wide state.
    pub fn state(&self) -> ReadyState {
        if self.peers.is_empty() {
            // Degenerate: no peers. Treat as pending — nothing to play against.
            return ReadyState::Pending;
        }
        let any_ready = self.peers.values().any(|r| *r);
        let all_ready = self.peers.values().all(|r| *r);
        if all_ready || (self.override_enabled && any_ready) {
            ReadyState::AllReady
        } else {
            ReadyState::Pending
        }
    }

    /// Iterator over peers that are currently not ready. Useful for the UI
    /// "waiting on …" display.
    pub fn not_ready(&self) -> impl Iterator<Item = &NodeId> {
        self.peers
            .iter()
            .filter_map(|(n, r)| if *r { None } else { Some(n) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(b: u8) -> NodeId {
        NodeId::from_bytes([b; 32])
    }

    #[test]
    fn empty_gate_is_pending() {
        let g = ReadyGate::new();
        assert_eq!(g.state(), ReadyState::Pending);
        assert!(g.is_empty());
    }

    #[test]
    fn single_peer_ready_is_all_ready() {
        let mut g = ReadyGate::new();
        g.set(n(1), true);
        assert_eq!(g.state(), ReadyState::AllReady);
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn single_peer_not_ready_is_pending() {
        let mut g = ReadyGate::new();
        g.set(n(1), false);
        assert_eq!(g.state(), ReadyState::Pending);
    }

    #[test]
    fn unanimity_required_by_default() {
        let mut g = ReadyGate::new();
        g.set(n(1), true);
        g.set(n(2), true);
        g.set(n(3), false);
        assert_eq!(g.state(), ReadyState::Pending);

        g.set(n(3), true);
        assert_eq!(g.state(), ReadyState::AllReady);
    }

    #[test]
    fn override_allows_single_ready_to_advance() {
        let mut g = ReadyGate::new();
        g.set(n(1), true);
        g.set(n(2), false);
        g.set(n(3), false);
        assert_eq!(g.state(), ReadyState::Pending);

        g.set_override(true);
        assert_eq!(g.state(), ReadyState::AllReady);
    }

    #[test]
    fn override_still_requires_at_least_one_ready() {
        let mut g = ReadyGate::new();
        g.set(n(1), false);
        g.set(n(2), false);
        g.set_override(true);
        assert_eq!(g.state(), ReadyState::Pending);
    }

    #[test]
    fn remove_peer_can_unblock() {
        let mut g = ReadyGate::new();
        g.set(n(1), true);
        g.set(n(2), false);
        assert_eq!(g.state(), ReadyState::Pending);

        g.remove(&n(2));
        assert_eq!(g.state(), ReadyState::AllReady);
    }

    #[test]
    fn not_ready_enumerates_laggards() {
        let mut g = ReadyGate::new();
        g.set(n(1), true);
        g.set(n(2), false);
        g.set(n(3), false);
        g.set(n(4), true);
        let laggards: Vec<_> = g.not_ready().copied().collect();
        assert_eq!(laggards, vec![n(2), n(3)]);
    }

    #[test]
    fn get_returns_current_state() {
        let mut g = ReadyGate::new();
        g.set(n(1), true);
        assert_eq!(g.get(&n(1)), Some(true));
        g.set(n(1), false);
        assert_eq!(g.get(&n(1)), Some(false));
        assert_eq!(g.get(&n(2)), None);
    }
}
