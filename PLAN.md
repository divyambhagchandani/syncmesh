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
| 3 | Sync state machine (`syncmesh-core`) | ✅ done |
| 4 | Full-mesh + end-to-end wiring in the binary | 🟡 local mpv control + media-id publication shipped; N>2 dialing still open |
| 5 | TUI + chat (`bin/syncmesh`) | ⬜ todo |
| 6 | Polish, packaging, release | ⬜ todo |

Workspace state: **170 tests passing** (96 core + 5 proptest + 4 simulator + 13 net + 6 loopback + 13 bin — plus 33 `syncmesh-player` tests that require a local mpv), clippy-clean with `pedantic` + `-D warnings`, CI matrix green on Linux/macOS/Windows. `syncmesh-core` line coverage: **97.77%** (measured via `cargo llvm-cov`).

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

Tests (105 in this crate):
- 96 unit tests across all modules.
- 5 proptest cases ([`tests/conflict_proptest.rs`](crates/syncmesh-core/tests/conflict_proptest.rs)) — randomized simultaneous-event scenarios.
- 4 multi-peer simulator tests ([`tests/mesh_simulator.rs`](crates/syncmesh-core/tests/mesh_simulator.rs)) — in-process 3-peer mesh with injected propagation delay.

**Coverage** (measured 2026-04-19 via `cargo llvm-cov`, **97.77%** overall):

| File | Lines |
|------|-------|
| `conflict.rs`, `media.rs`, `time.rs` | 100% |
| `drift.rs` | 100% |
| `protocol.rs` | 100% |
| `ready.rs` | 97.25% |
| `rtt.rs` | 95.83% |
| `node_id.rs` | 93.62% |
| **`state.rs`** | **97.31%** |

**Exit criterion met:** the simulator runs 3-peer scenarios with configurable link delay and asserts end-to-end sync properties; the state machine is testable without network or mpv; every `state.rs` file is above the >90% bar.

The previously-flagged gaps in `state.rs` (inbound `ControlAction::{Play, Seek, MediaChanged}`, every `PresenceEvent` variant on the inbound path, local non-Pause control, heartbeat-mismatch-with-no-local-media) are now each covered by a focused unit test in [`state.rs`](crates/syncmesh-core/src/state.rs).

---

## Part B — What's left

### 🟡 Phase 4 — Full mesh + end-to-end wiring

The three library crates are wired together in [`bin/syncmesh`](bin/syncmesh/src/). The binary compiles clippy-clean with `pedantic + -D warnings`, its `--help` round-trips, and the event-loop skeleton dispatches every `RoomState::Output` variant.

**Shipped:**

- [`cli.rs`](bin/syncmesh/src/cli.rs) — `clap`-driven `create` / `join <ticket>` subcommands plus global `--nickname`, `--no-mpv`, `--mpv-binary`.
- [`config.rs`](bin/syncmesh/src/config.rs) — `ProjectDirs`-based config dir resolution; holds the persistent `identity.key`.
- [`peer_task.rs`](bin/syncmesh/src/peer_task.rs) — per-peer control reader + datagram reader + writer-task trio, plus the `accept_next` loop.
- [`app.rs`](bin/syncmesh/src/app.rs) — single `tokio::select!` owning the `RoomState`. Funnels `MpvEvent`s, inbound frames, peer-lifecycle events, and a 1 Hz `Tick` through `RoomState::apply`, dispatching `Output::Broadcast` / `SendTo` / `Mpv(..)` / `Notify` appropriately. Heartbeats go out as QUIC datagrams; control frames via per-peer writer mpsc.
- [`echo.rs`](bin/syncmesh/src/echo.rs) — per-property echo guard (pause/seek/speed) armed inside `send_mpv` before each outbound `MpvCommand`. First matching mpv edge within a 1.5 s window is suppressed; edges outside the window or that don't match become genuine `LocalControl` broadcasts. Seek matching uses a ±1 s keyframe-snap tolerance; pause and speed are exact.
- [`media.rs`](bin/syncmesh/src/media.rs) — coalesces the three independent mpv property streams (`filename`, `duration`, `file-size`) into a single `MediaId` and emits `Input::LocalMediaChanged` on each distinct file. A fresh `filename` resets the duration + size so stale values from the previous file never leak into a new `MediaId`.
- [`main.rs`](bin/syncmesh/src/main.rs) — `tracing-subscriber` init, identity load, `MeshEndpoint::bind`, optional mpv spawn, Ctrl-C handler, accept task spawn, ticket print (create) / dial (join).

**Local mpv edge → broadcast wiring:**

- `MpvEvent::Pause(p)` → `EchoGuard::consume_pause`; on miss, broadcast `LocalControl::{Pause,Play}`.
- `MpvEvent::Seeking` arms `seek_in_progress`; `MpvEvent::PlaybackRestart` flips it to `seek_just_completed`; the next `MpvEvent::TimePos(s)` is evaluated as a seek target (suppress via `EchoGuard::consume_seek`, or broadcast `LocalControl::Seek`). Any other `TimePos` is passive playback progress → `MpvStateUpdate`, no broadcast.
- `MpvEvent::Speed(s)` → `EchoGuard::consume_speed`; on miss, broadcast `LocalControl::SetSpeed`.
- `MpvEvent::{Filename, Duration, FileSize}` → `MediaCollector`; when all three are present and differ from the last emission, emit `LocalMediaChanged`.

Drift-correction `SetSpeed` / `Seek` commands also pass through `send_mpv` and therefore arm the echo guard, so the mpv property-change events they generate never rebroadcast.

**Remaining gap:**

1. **N>2 dialing is one-hop only.** On `join`, we dial exactly the host from the ticket. We don't re-dial the rest of the mesh from a subsequent `PresenceEvent::PeerList`, because `PeerList` currently carries only `(NodeId, String)` pairs and we'd need each peer's `EndpointAddr` to dial. Plan: either (a) extend `PeerList` to carry `EndpointAddr`s (postcard-opaque bytes would keep the core crate iroh-free), or (b) relay control frames through the host for N>2 rather than full-meshing — revisit once we have real 3-peer dogfooding data.

**Exit criterion (unchanged):** three peers on three machines (mixed OS) watch the same local file; pause/play/seek propagate in <50 ms on LAN; drift stays under 100 ms in steady state. The 2-peer path should now satisfy this end-to-end; full-mesh N>2 remains blocked on gap 1 above.

**Dependencies pulled in:** `clap` (derive), `directories` (config paths), `tracing-subscriber`, `anyhow`, `iroh` (for `EndpointAddr` on the dial path).

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
