# syncmesh — implementation plan (living document)

**A P2P, mesh-based Syncplay alternative for mpv.**

Stack: Rust 2024 + tokio + iroh + mpvipc. Single static binary per OS, no runtime dependencies beyond a user-installed mpv.

---

## Progress dashboard

| Phase | Scope | Status |
|-------|-------|--------|
| 0 | Scaffolding (workspace, CI, toolchain, tracing) | ✅ done |
| 1 | mpv integration (`syncmesh-player`) | ✅ done |
| 2 | Two-peer mesh over iroh (`syncmesh-net`) | ✅ done |
| 3 | Sync state machine (`syncmesh-core`) | 🟡 meets overall spec; `state.rs` has known gaps |
| 4 | Full-mesh + end-to-end wiring in the binary | ⬜ next |
| 5 | TUI + chat (`bin/syncmesh`) | ⬜ todo |
| 6 | Polish, packaging, release | ⬜ todo |

Workspace state: **142 tests passing**, clippy-clean with `pedantic` + `-D warnings`, CI matrix green on Linux/macOS/Windows. `syncmesh-core` line coverage: **91.94%** (measured via `cargo llvm-cov`).

Pinned decisions: see [Decision index](#decision-index) at the bottom — the 22 locked-in choices from the original spec. Nothing there has changed.

---

## Part A — What's been shipped

### ✅ Phase 0 — Scaffolding

- Cargo workspace with `resolver = "3"` and edition 2024 across four members:
  - `crates/syncmesh-core`
  - `crates/syncmesh-net`
  - `crates/syncmesh-player`
  - `bin/syncmesh`
- [`rust-toolchain.toml`](rust-toolchain.toml) pins stable + rustfmt/clippy.
- [`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs fmt + clippy + test on ubuntu-latest, macos-latest, windows-latest with `RUSTFLAGS: -D warnings`.
- Workspace-level clippy config in [`Cargo.toml`](Cargo.toml) enables `pedantic` with `missing_errors_doc` / `module_name_repetitions` allowed.
- `tracing` + `tracing-subscriber` wired into dev-deps.

**Exit criterion met:** `cargo test --workspace` green on all three platforms.

---

### ✅ Phase 1 — mpv integration (`syncmesh-player`)

Files:
- [`src/process.rs`](crates/syncmesh-player/src/process.rs) — spawns `mpv --input-ipc-server=<socket>` and supervises the child.
- [`src/transport.rs`](crates/syncmesh-player/src/transport.rs) — platform abstraction over Unix sockets vs Windows named pipes.
- [`src/ipc.rs`](crates/syncmesh-player/src/ipc.rs) — JSON IPC client, command dispatch, event pump.
- [`src/event.rs`](crates/syncmesh-player/src/event.rs) — translates raw mpv property changes into the typed `MpvEvent` enum (all 9 events from decision 18: `pause`, `time-pos`, `seeking`, `playback-restart`, `eof-reached`, `speed`, `filename`, `duration`, `file-size`).
- [`src/command.rs`](crates/syncmesh-player/src/command.rs) — typed `MpvCommand` → JSON renderer.

Tests:
- 30 unit tests (in-memory IPC with a mock mpv speaking the JSON wire protocol).
- 3 real-mpv tests ([`tests/real_mpv.rs`](crates/syncmesh-player/tests/real_mpv.rs)) covering pause/play/seek/speed round-trip, missing-binary error, and clean child-kill on handle drop.

**Exit criterion met:** a test binary opens a local file, drives pause/play/seek from Rust, and receives events back — cross-platform.

---

### ✅ Phase 2 — Two-peer mesh over iroh (`syncmesh-net`)

Shipped in this session. Iroh pinned at `0.98` with `tls-ring` (the planned `iroh = "1"` doesn't exist yet; expect a small upgrade pass when 1.0 lands — see risk register).

Files:
- [`src/identity.rs`](crates/syncmesh-net/src/identity.rs) — persistent 32-byte Ed25519 secret, stored raw on disk with 0600 perms on Unix. `load_or_create(path)` is idempotent.
- [`src/ticket.rs`](crates/syncmesh-net/src/ticket.rs) — versioned `syncmesh1<base32-nopad>` encoding of `iroh::EndpointAddr`. Round-trips pubkey + direct IP addrs + relay URLs. Case/whitespace tolerant on decode.
- [`src/framing.rs`](crates/syncmesh-net/src/framing.rs) — 4-byte BE length prefix + postcard payload, 64 KiB cap. Distinguishes clean EOF from mid-prefix truncation.
- [`src/peer.rs`](crates/syncmesh-net/src/peer.rs) — `PeerLink`: reliable bidi control stream + QUIC datagram channel + per-path RTT sampler feeding `RttEstimator`. Sends a one-byte `PROTOCOL_HELLO` on open so the acceptor's `accept_bi()` unblocks (iroh/QUIC doesn't materialize a fresh stream on the peer until data flows).
- [`src/mesh.rs`](crates/syncmesh-net/src/mesh.rs) — `MeshEndpoint`: iroh `Endpoint` bound with ALPN `b"syncmesh/0"`. Two presets: `MeshConfig::default()` (n0 relays + pkarr DNS) and `MeshConfig::localhost()` (Minimal crypto provider, `RelayMode::Disabled`, binds `127.0.0.1:0` — used by tests, not production).

Tests ([`tests/loopback.rs`](crates/syncmesh-net/tests/loopback.rs), 6/6 passing over the real iroh stack):
1. Control frame round-trips on the bidi stream.
2. 50 chat frames preserve send order.
3. Heartbeat `StateHeartbeat` round-trips as a QUIC datagram (with retry, since datagrams are lossy).
4. Ticket `encode` → `decode` → dial → frame exchange works end-to-end.
5. `RttEstimator` sees a real sample from transport telemetry after traffic flows (loopback ~3 ms).
6. Clean remote close surfaces as a `recv_frame` error, not a hang.

Unit tests (13 in the crate): identity round-trip, ticket encode/decode/prefix/version/base32 edge cases, framing EOF vs truncation vs oversized-length.

**Exit criterion met:** two endpoints in one process exchange `ControlEvent`s, `Heartbeat`s, and chat frames over the real iroh QUIC stack; RTT is visible; disconnect is graceful.

**Subtlety worth remembering:** iroh's `accept_bi()` only fires once the opener actually writes bytes, so `PeerLink::open` writes a hello byte immediately. Without it, the handshake deadlocks.

---

### 🟡 Phase 3 — Sync state machine (`syncmesh-core`)

Files (all pure logic, zero I/O, zero iroh dependency):
- [`src/protocol.rs`](crates/syncmesh-core/src/protocol.rs) — `Frame`, `ControlEvent`, `StateHeartbeat`, `PresenceEvent`, `ChatMessage`, `MediaId`, postcard encode/decode.
- [`src/node_id.rs`](crates/syncmesh-core/src/node_id.rs) — 32-byte Ed25519 pubkey with lexicographic `Ord` for conflict tiebreaking.
- [`src/rtt.rs`](crates/syncmesh-core/src/rtt.rs) — EWMA (α=0.15, matches Syncplay's `avrRtt`).
- [`src/conflict.rs`](crates/syncmesh-core/src/conflict.rs) — `(origin_ts, origin_id)` tiebreaker with 500 ms `CONFLICT_WINDOW_MS`.
- [`src/drift.rs`](crates/syncmesh-core/src/drift.rs) — tiered correction: `>1000 ms` hard seek, `>100 ms behind` slowdown (`speed=0.95`), `>100 ms ahead` speedup (`speed=1.05`).
- [`src/ready.rs`](crates/syncmesh-core/src/ready.rs) — unanimity gate with optional override.
- [`src/media.rs`](crates/syncmesh-core/src/media.rs) — `(filename, size, duration)` match with non-blocking mismatch surface.
- [`src/state.rs`](crates/syncmesh-core/src/state.rs) — the central `apply(Input) -> Vec<Output>` pure function. Owns the room state machine, chat ring buffer (200 msgs), per-peer heartbeat table, dedup by `(origin, seq)`, late-joiner auto-seek-then-Ready-gate flow.
- [`src/time.rs`](crates/syncmesh-core/src/time.rs) — `Clock` trait with `SystemClock` + `MockClock` for deterministic simulator time.

Tests (90 in this crate):
- 81 unit tests across all modules.
- 5 proptest cases ([`tests/conflict_proptest.rs`](crates/syncmesh-core/tests/conflict_proptest.rs)) — randomized simultaneous-event scenarios.
- 4 multi-peer simulator tests ([`tests/mesh_simulator.rs`](crates/syncmesh-core/tests/mesh_simulator.rs)) — in-process 3-peer mesh with injected propagation delay.

**Coverage** (measured 2026-04-19 via `cargo llvm-cov`, 91.94% overall — passes the plan's >90% bar):

| File | Lines |
|------|-------|
| `conflict.rs`, `media.rs`, `time.rs` | 100% |
| `drift.rs`, `protocol.rs` | ~100% |
| `ready.rs` | 97.25% |
| `rtt.rs` | 95.83% |
| `node_id.rs` | 93.62% |
| **`state.rs`** | **87.15%** (119 uncovered lines — below bar) |

**Exit criterion status:** the simulator runs 3-peer scenarios with configurable link delay and asserts end-to-end sync properties; the state machine is testable without network or mpv. Overall coverage meets spec, but `state.rs` individually is below 90%.

**Known gaps in `state.rs`** (deferred to the follow-up task below):

- Inbound `ControlAction::{Play, Seek, SetSpeed, MediaChanged}` handling (lines 459–497). Only `Pause` is exercised by the simulator.
- Every `PresenceEvent` variant on the inbound path (lines 550–592): `Join`, `Leave`, `Ready`, `Rename`, `PeerList`.
- Local `on_local_control` for non-Pause actions (lines 614–622).
- Heartbeat media-mismatch branch when local has no media set (lines 537–543).

These are not defensive branches — they're protocol paths Phase 4 will exercise every session. Closing them before Phase 4 avoids inheriting latent bugs.

**Follow-up task (before or alongside Phase 4):** add ~6–8 focused unit tests covering the gaps above. Estimated 30–45 minutes; target >90% on `state.rs` in isolation.

---

## Part B — What's left

### ⬜ Phase 4 — Full mesh + end-to-end wiring (~1 week)

The three library crates all exist and are tested in isolation. Phase 4 wires them into a running process that actually watches a video with a friend.

**Work to do, in the `bin/syncmesh` crate:**

1. **Task topology (per §3.2 of the original plan).** Spawn these as tokio tasks, all funneling into a single `event_loop_task` that owns the authoritative `RoomState`:
   - `mpv_event_task` — reads `MpvEvent`s from `syncmesh-player`, forwards to the event loop.
   - `mpv_cmd_task` — consumes `MpvCommand`s from the event loop, dispatches via `syncmesh-player`.
   - `mesh_acceptor_task` — `MeshEndpoint::accept_next` loop, spawns a reader/writer task pair per new `PeerLink`.
   - `per_peer_reader_task` / `per_peer_writer_task` — one pair per peer; decode/encode `Frame`s on each side.
   - `heartbeat_task` — 1 Hz `StateHeartbeat` fan-out via `PeerLink::send_datagram`.
   - `drift_task` — 1 Hz `check_drift` per known peer, emits `SetSpeed`/`Seek` commands.
   - `event_loop_task` — single `select!` over all inputs.

2. **N>2 mesh bootstrap.** On join, the first peer we dial sends us a `PresenceEvent::PeerList`; we then dial each additional peer in parallel. Implement peer-churn: handle `Leave`, reconnect-with-backoff on transient errors.

3. **Graceful shutdown.** Wire `MeshEndpoint::close().await` and `syncmesh-player` child kill into a single `Ctrl-C` handler.

**Exit criterion:** three peers on three machines (mixed OS) watch the same local file. Pause/play/seek propagate in <50 ms on LAN. Drift stays under 100 ms in steady state.

**Dependencies ready to pull in:** `tokio::sync::{mpsc, broadcast}`, `directories` (for config path), `clap` (CLI args).

---

### ⬜ Phase 5 — TUI and chat (~1 week)

- ratatui layout: peer list (with ready dots, RTT, drift), status bar (media info), chat pane, input line.
- Keyboard shortcuts: `r` toggle ready, `c` copy ticket, `space` relay pause/play, `/` enter chat, `q` quit.
- Chat ring buffer rendered from `RoomState::chat_ring` with scrollback.
- Ticket input screen at startup: "[c]reate room" vs "[j]oin room <paste ticket>".

**Exit criterion:** a new user launches syncmesh, creates a room, shares the ticket through any channel, and a friend pastes it and joins — all without reading docs.

---

### ⬜ Phase 6 — Polish, packaging, release (~1–1.5 weeks)

- Config file (TOML) at `directories`-resolved paths, including the self-hosted relay key.
- `--verbose` / `--log-file` CLI flags.
- GitHub Release CI: cross-compiled binaries for `x86_64-linux`, `aarch64-linux`, `x86_64-macos`, `aarch64-macos`, `x86_64-windows` via `cargo-zigbuild`.
- macOS codesign + notarization (or documented Gatekeeper bypass if no dev cert).
- Homebrew tap, winget manifest, AUR PKGBUILD (community-contributable).
- README with screencast + firewall/router FAQ.

**Exit criterion:** v0.1.0 released; single binary <15 MB per platform; first external user completes the invite flow without help.

---

## Part C — Protocol spec (reference, fully implemented)

The full wire protocol is in [`syncmesh-core::protocol`](crates/syncmesh-core/src/protocol.rs). Ticket format is in [`syncmesh-net::ticket`](crates/syncmesh-net/src/ticket.rs). Length-prefix framing on control streams is in [`syncmesh-net::framing`](crates/syncmesh-net/src/framing.rs). Datagrams carry a naked postcard-encoded `Frame`.

ALPN: `b"syncmesh/0"`. Heartbeat interval: 1 Hz. Conflict window: 500 ms. Drift tiers: 100 ms / 1000 ms. Chat ring: 200 messages. All as originally decided.

---

## Decision index

Unchanged from the original spec — kept here for quick lookup.

| # | Decision | Implemented where |
|---|----------|------|
| 1 | Rust 2024 edition | workspace `Cargo.toml` |
| 2 | tokio multi-thread | all tests use `#[tokio::test(flavor = "multi_thread")]` where needed |
| 3 | Transport: iroh 0.98 (pinned; 1.x when released) | `syncmesh-net/Cargo.toml` |
| 4 | Full mesh, no gossip at N≤15 | Phase 4 |
| 5 | Control stream + datagram heartbeat | `syncmesh-net::peer::PeerLink` |
| 6 | postcard wire format | `syncmesh-core::protocol::Frame::{encode,decode}` |
| 7 | Room = host NodeId | ticket embeds `EndpointAddr` directly |
| 8 | Equal-peer mesh, deterministic tiebreaker | `syncmesh-core::conflict` |
| 9 | Auth = ticket possession | `syncmesh-net::ticket` |
| 10 | Persistent Ed25519 identity | `syncmesh-net::identity::load_or_create` |
| 11 | Media id = `(filename, size, duration)` | `syncmesh-core::media` |
| 12 | Late joiner: auto-seek then Ready gate | `syncmesh-core::state` |
| 13 | Ready unanimity with override | `syncmesh-core::ready::ReadyGate` |
| 14 | Drift tiers 100 ms / 1000 ms | `syncmesh-core::drift` |
| 15 | Clock = origin + RTT/2 EWMA | `syncmesh-core::rtt::RttEstimator` |
| 16 | mpv spawn by default, Lua script power mode | `syncmesh-player::process` (default done; Lua mode = Phase 6) |
| 17 | mpvipc crate | `syncmesh-player/Cargo.toml` |
| 18 | 9 mpv events | `syncmesh-player::event` |
| 19 | ratatui v1, egui v1.2 | Phase 5 |
| 20 | Ticket paste, QR v1.1 | ticket done; QR deferred |
| 21 | n0 relay default, self-host option | `MeshConfig` (self-host key = Phase 6 config file) |
| 22 | Chat on control stream, 200-msg ring | `syncmesh-core::state` chat ring buffer |

---

## Risk register (current)

| Risk | Status | Notes |
|------|--------|-------|
| iroh 1.0 API churn | **realized** | Had to pin 0.98; API renamed `NodeId`→`EndpointId`, `NodeAddr`→`EndpointAddr`. Budget the planned 2-day upgrade pass when 1.0 ships. |
| mpv IPC latency on Windows named pipes | mitigated | Phase 1 tests pass on Windows; no latency issue seen yet. Benchmark in Phase 4 under load. |
| Hole-punching rate in real networks | open | Relay fallback already works (iroh handles it); real-world numbers collected in Phase 6 dogfooding. |
| ratatui UX rejection | open | v1 ships TUI; egui port remains a v1.2 contingency. |
| n0 public relay shutdown | mitigated | Self-host configuration key is in the `MeshConfig` shape already. |
| Symmetric-NAT with unreachable relay | open | Accept and document; no code mitigation planned. |
| mpv major-version IPC break | very low | Pin mpv version in docs at Phase 6. |
| Syncplay brand confusion | mitigated | Name chosen distinctly; protocol credit in README. |

---

## Open questions for Phase 3–4 dogfooding

(Still open; revisit once a real mesh is running.)

1. Does the `speed=1.05` speedup tier cause audible pitch shift worse than the 200-ms-ahead it corrects? Cheap A/B once Phase 4 lands.
2. Should late joiners see room chat history? Currently `RoomState` keeps a 200-message ring; deciding whether to replay to new peers affects `PresenceEvent::Join` handling.
3. Human-memorable invite (BIP-39-style word list) as an alternative to base32 tickets — deferred to v1.2.
4. "DJ mode" / remote control of another peer's mpv — deliberately out of scope for v1.

---

*End of plan. Phases 0–3 are shipped and verified. Phase 4 is next up.*
