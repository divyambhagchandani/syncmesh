# syncmesh — remaining-work roadmap

Companion to [PLAN.md](PLAN.md). Where PLAN.md is a status board, this document is the detailed forward-looking plan for everything left: the last Phase 4 gap, all of Phase 5, and all of Phase 6. Each section opens with the spec decisions it interacts with (numbered per [PLAN.md Decision index](PLAN.md#decision-index)), then describes the concrete work, the tradeoffs, and the exit criterion.

> Decisions from the original spec are **frozen** unless called out explicitly. When a decision constrains an implementation choice, it's cited inline as "(decision N)" so a reader can trace intent back to the source.

---

## Table of contents

- [Where we are](#where-we-are)
- [Phase 4 — remaining: N>2 full-mesh dialing](#phase-4--remaining-n2-full-mesh-dialing)
- [Phase 5 — TUI + chat](#phase-5--tui--chat)
- [Phase 6 — polish, packaging, release](#phase-6--polish-packaging-release)
- [Cross-cutting risks](#cross-cutting-risks)
- [Suggested sequencing + cadence](#suggested-sequencing--cadence)

---

## Where we are

Phases 0–3 are shipped with full test coverage. Phase 4 has shipped everything except N>2 full-mesh dialing — the binary runs end-to-end with two peers: pause/play/seek propagate, drift correction fires, heartbeats flow, echoes are suppressed, `MediaId`s are published. The state machine is pure, clippy-clean with `pedantic + -D warnings`, 97.77% line coverage. Workspace currently at 170 passing tests (137 always + 33 requiring mpv).

What remains is, in order:

1. One Phase 4 item — **N>2 dialing**. Without it, any joined peer talks only to the host, which breaks decision 8's equal-peer mesh and inflates drift-correction latency for peers ≥3.
2. All of Phase 5 — **TUI + chat**. Everything the user sees. Keybindings, chat, ticket UX, ready-state display, RTT + drift indicators.
3. All of Phase 6 — **polish, packaging, release**. Config file, cross-compiled binaries, signing, distribution, self-hosted relay plumbing, Lua power-user mode, QR-code invites, README.

Two estimate anchors from the original spec: Phase 4 remainder ≈ 2–3 days of work, Phase 5 ≈ 5–7 days, Phase 6 ≈ 7–10 days. Total ~3 weeks of focused work, not counting a deliberate dogfooding window between Phase 5 and Phase 6.

---

## Phase 4 — remaining: N>2 full-mesh dialing

### Context

Three decisions from the spec constrain this work:

- **Decision 3** — transport is iroh, which means peers connect directly by `EndpointId` once they can resolve an `EndpointAddr` (pubkey + direct IPs + optional relay URL).
- **Decision 4** — topology is a **full mesh at N ≤ 15**, not gossip. Every peer pair has its own QUIC connection.
- **Decision 8** — after bootstrap, the host has **no special runtime role**. If the host disconnects, the mesh keeps working because every peer is equal.

Today's binary satisfies (3) but violates (4) and (8) at N ≥ 3:

- A joiner dials the host out of the ticket and opens a `PeerLink`.
- On `PeerConnected`, the state machine emits `PresenceEvent::PeerList` to the joiner. The `PeerList` enumerates everyone in the room, but carries only `(NodeId, String)` — **no `EndpointAddr`** — so the joiner has nothing to dial with. It records the roster but doesn't open new connections.
- Result: peer C joining a {host, A} room ends up connected only to the host. Heartbeats from A never reach C directly (no link), so C can't compute per-peer RTT to A, can't use A as a drift reference, and any control event originating at A transits through two hops (A → host → C). If the host then drops, C and A are mutually invisible until one of them reconnects.

### Two options (already noted in PLAN.md)

**Option A — extend `PresenceEvent::PeerList` to carry addresses.**

Change the wire type from

```rust
PeerList { peers: Vec<(NodeId, String)> }
```

to

```rust
PeerList { peers: Vec<(NodeId, String, Vec<u8>)> }  // third = postcard-encoded EndpointAddr
```

The `Vec<u8>` is deliberately opaque to the core crate — keeps `syncmesh-core` iroh-free per the crate-split invariant (§3.1 of the spec). Net/bin encodes and decodes.

Pros: matches decisions 4 and 8; minimal latency overhead (one hop per control event in steady state); host becoming unreachable no longer splits the mesh.
Cons: wire change (fine, pre-release); someone has to own an address registry.

**Option B — relay control frames through the host.**

Keep today's star topology. Host becomes a logical hub. N>2 peers don't directly connect; control events go peer → host → peer.

Pros: no protocol change.
Cons: violates decision 8; doubles control-event latency (~18 ms target → ~36 ms on healthy links); host crash splits the mesh mid-session; can't easily stream per-peer heartbeats to every other peer without either flooding or additional protocol surface.

**Recommendation: Option A.** The protocol cost is small, it honors decisions 4 and 8, and the latency cost of B gets visibly worse on transatlantic links where RTT is ~150 ms per hop.

### Work items for Option A

1. **Protocol change** (~30 lines):
   - Modify `syncmesh_core::PresenceEvent::PeerList` to the 3-tuple shape above.
   - Update the `assert_roundtrip` test in `protocol.rs` and the existing `inbound_peer_list_*` tests in `state.rs`.
   - Because the core crate doesn't know what's inside the `Vec<u8>`, the state machine's responsibility ends at storing/echoing the bytes; decoding happens in the bin layer.

2. **Address registry in the bin** (~80 lines, probably a new module `bin/syncmesh/src/addrs.rs`):
   - `HashMap<NodeId, Bytes>` where `Bytes` is a postcard-encoded `iroh::EndpointAddr`.
   - Self-entry seeded from `MeshEndpoint::addr()` at startup.
   - On outbound dial: we already have the callee's `EndpointAddr` (it came from the ticket or a prior PeerList decode), record it.
   - On accepted inbound connection: we know the callee's `NodeId` via `Connection::remote_id()`, but the full `EndpointAddr` (direct IP list + relay URL) isn't trivially exposed. Two plumbing options:
     - (a) Introduce a new `PresenceEvent::AddrAnnounce { node, addr_bytes }` that every peer broadcasts once on connect. Receivers update the registry. Cleanly decoupled from `PeerList`.
     - (b) Ask iroh directly via `Endpoint::remote_info(remote_id)` which returns the current paths. API-fragile but zero extra frames.
   - Prefer (a) — self-reported `EndpointAddr` is authoritative and doesn't depend on iroh internals. Cost is one 100-byte frame per peer per session.

3. **Bin-layer PeerList generation** (~40 lines):
   - `RoomState::on_peer_connected` currently emits PeerList from inside the state machine. With the address bytes living in the bin, the state machine no longer has what it needs. Two refactors are viable:
     - Push a `RoomState::emit_peer_list(to: NodeId, addrs: &AddrRegistry) -> Frame` helper the bin calls after `apply`.
     - Or: move PeerList construction out of the state machine entirely and into a bin-layer helper that runs after every `Output::Notify` style signal from core. Cleaner separation but introduces a new Output variant.
   - Lean toward the helper — smaller diff, keeps the state machine pure.

4. **Dialing unknown peers from an inbound PeerList** (~50 lines in `app.rs`):
   - `on_peer_frame` decodes `PresenceEvent::PeerList`. For each `(node, nickname, addr_bytes)`:
     - Skip if `node == self.local` or already in `self.peers`.
     - Decode `addr_bytes` via postcard into `EndpointAddr`.
     - Dedupe against in-flight dials (a `HashSet<NodeId>` of pending connects).
     - `tokio::spawn` a dial task: `mesh.dial(addr).await` → on success, send a `LoopEvent::PeerConnected { link }` into the main loop, mirroring the accept path.
   - Failure semantics: dial errors just log and back off. Retries happen when the next PeerList or AddrAnnounce arrives.

5. **PeerList refresh on roster changes**:
   - When a new peer joins, *existing* peers send PeerList to the joiner (already implemented). The joiner then dials each listed peer. But if the joiner's address has changed since the host bound, the existing peers need to learn the joiner's new addr too.
   - Simplest: AddrAnnounce-on-connect from the joiner solves it — the joiner announces itself to everyone in its very first frames.

6. **Integration test** (~80 lines, a new `bin/syncmesh/tests/mesh_3peer.rs` or in `syncmesh-net/tests/`):
   - Three in-process `MeshEndpoint::localhost()` endpoints.
   - A creates; B joins with A's ticket; C joins with B's ticket.
   - Assert: within a 2 s timeout, A ↔ C have a direct `PeerLink` and a test ControlEvent originated at A reaches C.
   - Optional stress: tear down B's endpoint; assert A ↔ C survive.

### Non-goals (deliberately deferred)

- Peer NAT-rebinding mid-session (a peer's EndpointAddr changing during a watch). Real — but in practice iroh's path manager handles this inside the Connection without touching our layer. Defer until dogfooding surfaces a real break.
- Address lookup via iroh's discovery (pkarr/DNS) as a fallback when AddrAnnounce hasn't arrived. Nice-to-have; decision 3's default discovery already makes this work on real internet, just not in the localhost-preset used for tests.
- Authorization of new peers (allow-lists). Decision 9 says ticket possession = room membership for v1; any peer we learn about via PeerList already has the ticket's trust signal transitively. Decision 10 notes friends-of-friends allow-lists as a *later* feature.

### Exit criterion

Unchanged from the original Phase 4 exit: three peers on three machines (mixed OS) watch the same local file; pause/play/seek propagates in under 50 ms on LAN; drift stays under 100 ms in steady state. With Option A shipped, a 3-peer mesh test passes and host-disconnect no longer splits the mesh.

---

## Phase 5 — TUI + chat

### Context

Four decisions govern this phase:

- **Decision 19** — UI is **ratatui** (TUI) for v1; egui is a v1.2 contingency if non-terminal-native users turn out to matter.
- **Decision 20** — invite UX is **ticket copy/paste**; QR rendering is v1.1.
- **Decision 22** — chat on the same per-pair control stream; ring buffer of **200 messages** in a side pane.
- **Decision 13** — ready state is per-peer, unanimity-gated with an optional override.

The audience self-selects: anyone willing to adopt a P2P Syncplay alternative is already comfortable in a terminal. We optimize for keyboard-heavy, dense, fast — not mouse-driven.

### Layout

```
┌──────────────────────────────────────────────────────────────────┐
│ syncmesh · movie.mkv · 01:23:45 / 02:15:00 · paused · 3 peers    │   status bar
├────────────────────────┬─────────────────────────────────────────┤
│ Peers                  │ Chat                                    │
│  ● you (me)      [R]   │ [10:15] alice: ready when you are       │
│    -    +12ms          │ [10:16] you:   same                     │
│  ● alice         [R]   │ [10:17] bob:   sec, getting popcorn     │
│    41ms   -3ms         │                                         │
│  ○ bob           [ ]   │                                         │
│    50ms  +200ms        │                                         │
│                        │                                         │
├────────────────────────┴─────────────────────────────────────────┤
│ > |                                                              │   input line
└──────────────────────────────────────────────────────────────────┘
  r ready   c copy ticket   space pause   / chat   tab override   q quit
```

- Left pane: peers, with a filled circle for "ready" and open for "not". Next line under each peer: per-peer RTT EWMA and signed drift (ms). Local peer shows its own ready state and no RTT.
- Right pane: chat ring buffer (200 entries, decision 22), scrollable with PgUp/PgDn.
- Status bar: local media, playback position / duration, paused/playing, peer count.
- Input line: chat or `:command`. Leading `/` also enters chat mode.
- Keyhints footer: always on, auto-truncates.

### Keybindings

From decision 20 and Syncplay tradition:

| Key      | Action                                                      |
|----------|-------------------------------------------------------------|
| `r`      | Toggle local ready (decision 13) — broadcasts `PresenceEvent::Ready` |
| `c`      | Copy the room ticket to clipboard                           |
| `space`  | Relay pause/play to mpv — emits `Input::LocalControl` so the mesh follows |
| `/`      | Enter chat input mode                                       |
| `Enter`  | Send chat (in chat mode) / relay `pause` toggle (otherwise) |
| `Esc`    | Exit chat mode, drop input                                  |
| `PgUp` / `PgDn` | Scroll chat pane                                      |
| `Tab`    | Toggle ready-gate override (decision 13, off by default)    |
| `q`      | Graceful quit (Ctrl-C still works)                          |
| `?`      | Help overlay with full keymap                               |

Left arrow / right arrow for timeline-nudge is *deliberately* not bound in v1 — mpv's native keybinds already handle this, and our echo guard will ensure the resulting seek broadcasts to the mesh. Less surface area, fewer surprises.

### Architecture: where rendering lives

The event loop (`App::run`) currently owns `&mut RoomState` and serializes all mutations. Rendering needs a read-only view on every frame without blocking the event loop. Two options:

- (A) Wrap `RoomState` in `Arc<RwLock<_>>`, render task holds a read guard.
- (B) After each `dispatch`, the event loop publishes a `RoomSnapshot` (cheap-clone view) to a `tokio::sync::watch` channel; render task reads the latest snapshot.

Option B is cleaner: no lock contention, single writer stays single-writer, snapshots are immutable so render logic is trivially reasoned about. Snapshots are small (≤N peers × ~200 bytes, plus 200 chat messages × ~100 bytes = ~25 KB total) — no GC pressure.

`RoomSnapshot` lives in `syncmesh-core::state` (or a new `snapshot.rs`). Fields: `local_node`, `local_nickname`, `local_playback`, `local_media`, `local_ready`, peers (`Vec<PeerSnapshot>`), chat (`VecDeque<ChatMessage>`), ready state, override flag.

### Work items

1. **`RoomSnapshot` + `RoomState::snapshot(&self) -> RoomSnapshot`** (~80 lines in `syncmesh-core`):
   - Pure clone; no behavior change.
   - Add `snapshot()` call after each `apply` in `app.rs`, publish on `watch::Sender<RoomSnapshot>`.

2. **`bin/syncmesh/src/ui/` module** (~500 lines total):
   - `ui/mod.rs` — public `run_ui(snapshot_rx: watch::Receiver<RoomSnapshot>, ui_tx: mpsc::Sender<UiEvent>) -> Result<()>`.
   - `ui/layout.rs` — ratatui widget trees for status bar, peer pane, chat pane, input line, help overlay.
   - `ui/keybinds.rs` — `crossterm::event::KeyEvent → Option<UiEvent>` mapping.
   - `ui/input.rs` — chat input state machine (insert, backspace, word-delete, enter, escape).

3. **New `UiEvent` type** bridging UI → event loop:
   ```rust
   enum UiEvent {
       ToggleReady,
       TogglePauseRelay,
       SubmitChat(String),
       ToggleOverride,
       CopyTicket,
       Quit,
   }
   ```
   These feed into `LoopEvent` variants (or directly invoke `Input::*`) in the main loop.

4. **Ticket copy** (`arboard` crate, optional dep behind `--no-default-features` escape):
   - On `CopyTicket`: `arboard::Clipboard::new()?.set_text(ticket)`.
   - Fallback: print ticket to a status-line notification (1 s flash) and write to stderr.
   - Linux without X/Wayland (pure TTY) returns an error — we degrade to the stderr fallback silently.

5. **Startup UX**:
   - Today we have `create` / `join <ticket>` subcommands (decision 20: "ticket paste"). Keep those as the authoritative entry points.
   - When invoked *without* a subcommand, enter a splash screen: "Create room [c]" / "Join room — paste ticket then Enter". This is the path Mom takes.
   - `esc` from splash = graceful quit.

6. **Render cadence**:
   - 30 FPS is decision-ish. Reality: 10 FPS is plenty for a mostly-static screen. Render only when either (a) the snapshot changes (watch `changed().await`) or (b) a 1 s timer fires for RTT/time-pos tickers. Saves CPU and battery on idle.

7. **Chat scrollback state** lives in the UI task, not `RoomState`:
   - A `usize` offset from the bottom of the ring.
   - Any new inbound chat auto-resets to 0 (follow mode), unless the user has scrolled — then it stays put.

8. **Help overlay** (`?` key):
   - Modal popup listing all keybindings with short descriptions. No pager — one screen, dismissable with any key.

### Testing strategy

Pure logic (input state machine, keybind mapping, snapshot projection) is unit-tested. The ratatui render tree is tested by rendering to a fixed-size `TestBackend` and asserting the resulting buffer text. No real terminal required.

Manual checklist before declaring Phase 5 done:
- macOS Terminal.app, iTerm2, Alacritty, Kitty, Windows Terminal, gnome-terminal — all render the layout without unicode breakage.
- Resize terminal from 80×24 to 200×60 mid-session — no panics.
- Clipboard copy works on macOS + X11. Fallback works on Wayland without wl-clipboard and on bare TTY.
- Chat input with IME (Japanese, Korean) — punt on full IME support, but don't crash.

### Exit criterion

Verbatim from the plan: a new user launches syncmesh, creates a room, shares the ticket by any means, and a friend pastes it and joins — all without reading docs. Demoable in under 90 seconds.

---

## Phase 6 — polish, packaging, release

### Context

Multiple decisions surface here:

- **Decision 16** — app-spawns mpv by default; Lua-script power mode is documented for users who launch mpv their own way.
- **Decision 20** — QR-code in-terminal rendering lands here (originally tagged "v1.1", folding into Phase 6 since the release vehicle exists).
- **Decision 21** — n0 relays are the default; self-hosted iroh-relay is a config key. Relay status is **not** surfaced in the UI.
- **Decision 3** — iroh is the transport. We're pinned at 0.98; **1.0 upgrade** lives in this phase.

### Work items

#### Config file

- Location: `directories::ProjectDirs::from("", "", "syncmesh").config_dir()/config.toml`.
- Schema (`serde`-deserialized into a `Config` struct):
  ```toml
  nickname         = "divyam"              # default peer nickname
  mpv_binary       = "/usr/bin/mpv"        # override mpv binary lookup
  mpv_spawn        = "auto"                # "auto" | "script" | "disabled"
  override_mode    = false                 # default state of ready-gate override
  relay            = "https://relay.example.com"  # optional — empty = use n0 defaults
  log_level        = "info"                # "error" | "warn" | "info" | "debug" | "trace"
  identity_path    = "identity.key"        # rel to config_dir; absolute also allowed
  ```
- Precedence: CLI flag > config file > default. Only `nickname`, `mpv_binary`, `log_level` are also CLI-exposed; rest live in the file only.
- On first run, if no config file exists, write a commented-out template to the config dir so the user discovers it.

#### Logging

- `--verbose` = raises default filter to `debug`.
- `--log-file <path>` = structured JSON logs via `tracing-appender` rolling daily. Useful for bug reports.
- Keep the `SYNCMESH_LOG` env var for power users (already works).
- Default stderr filter stays `info,syncmesh=debug` so dogfooding sessions have enough context without being noisy.

#### Self-hosted relay (decision 21)

- Extend `MeshConfig` with `relay_override: Option<RelayUrl>`.
- Plumb through `Endpoint::builder(...).relay_conf(...)` using whatever shape iroh exposes — need to re-check against the current 0.98 API; in 1.0 the shape may rename.
- No UI surface (decision 21 explicitly: hidden from the UI). Just a config-file line.
- README section documents how to stand up an `iroh-relay` and point the config at it. Reference iroh's own deployment docs.

#### QR-code invites (decision 20, "v1.1" in spec, folding in)

- `qrcode = "0.14"` crate, optional dep behind a `qr` feature flag.
- On `create`, print the text ticket first, then the QR underneath rendered with Unicode half-blocks (each QR pixel = half a terminal row). Gated with `--qr` or `qr = true` in config so users on terminals that mangle unicode blocks aren't confused.
- Future (v1.2): mobile companion that scans the QR and hands off to a desktop mpv/syncmesh — out of scope here.

#### Lua-script power mode (decision 16)

- A small Lua file (~40 lines) shipped in `scripts/syncmesh.lua`:
  - Uses mpv's `mp.utils.subprocess` or the builtin `mpv-ipc` to expose an IPC socket at a fixed, predictable path.
  - Sets the nine observed properties (decision 18) so syncmesh sees events even though it didn't spawn mpv.
- Binary support: `mpv_spawn = "script"` in config or `--no-spawn` CLI flag. When enabled, we skip `spawn()` and just `connect_transport(ipc_path)` against the script-side socket.
- Documented as the "I launch mpv my own way" workflow in README.

#### Cross-compile matrix

Targets per plan:

| Target                         | Host       | Tool              |
|--------------------------------|------------|-------------------|
| `x86_64-unknown-linux-musl`    | Linux      | `cargo-zigbuild`  |
| `aarch64-unknown-linux-musl`   | Linux      | `cargo-zigbuild`  |
| `x86_64-apple-darwin`          | macOS      | native            |
| `aarch64-apple-darwin`         | macOS      | native            |
| `x86_64-pc-windows-msvc`       | Windows    | native            |

Static musl targets on Linux keep the binary portable across distros. macOS universal2 is a nice-to-have; two separate arch binaries are acceptable for v0.1.0.

#### Signing + notarization

- **macOS**: requires an Apple Developer ID ($99/yr). Build → `codesign --sign "Developer ID Application: ..."` → zip → `xcrun notarytool submit` → `xcrun stapler staple`. Cost of admission; if we don't pay for a cert, README ships explicit `xattr -r -d com.apple.quarantine ./syncmesh` instructions and a brief Gatekeeper explainer.
- **Windows**: a code-signing cert is more expensive (~$400+/yr OV) and winget catalog requires signed binaries. For v0.1.0, ship unsigned; publish the sha256 on releases; direct-download still works with a SmartScreen warning. Defer winget catalog submission until a cert is in place.
- **Linux**: no signing. Publish sha256 checksums + an ASCII-armored signature from the maintainer's GPG key, advertised in the README.

#### Release CI

- Trigger on tag `v*`.
- Matrix job builds all five targets. macOS signs + notarizes if `APPLE_ID` / `APPLE_APP_PASSWORD` / `APPLE_TEAM_ID` secrets are present; Windows signs if `WINDOWS_CERT_BASE64` / `WINDOWS_CERT_PASSWORD` are set; otherwise skip with a warning.
- Upload artifacts + `SHA256SUMS` to the GitHub Release.
- Post-hook updates the Homebrew tap formula (bumps version + sha256) by pushing to a tap repo.
- Post-hook updates the winget manifest via `winget-create` (optional until signing is in place).

#### Distribution channels

- **GitHub Releases** — canonical source. Raw tarballs, sha256, optional .sig.
- **Homebrew tap** — `brew tap <user>/syncmesh && brew install syncmesh`. Lightest-touch macOS + Linux path.
- **winget** — once signing is sorted. Adds Windows one-liner install.
- **AUR** — `syncmesh-bin` PKGBUILD pulling the musl static binary; community-maintainable.
- **Install script** — `curl -sSL https://syncmesh.sh | sh` that detects OS/arch, downloads, verifies checksum, drops into `~/.local/bin`. Ship last; requires a domain.

#### README

- Screencast at the top (asciinema .cast or animated WebM).
- Quickstart: install → `syncmesh create --file movie.mkv` → share ticket → friend runs `syncmesh join <ticket> --file movie.mkv`.
- FAQ:
  - "mpv isn't on PATH" — config file example.
  - "We're behind double-NAT / symmetric NAT" — iroh handles it via relay; no user action required; if still broken, VPN or mobile hotspot.
  - "Can I use VLC?" — no (decision intentional).
  - "Does Netflix/etc work?" — no (decision intentional).
  - "Files don't match" — mismatch is non-blocking (decision 11); if intentional (different releases), ignore the warning.
  - "Firewall / router ports" — iroh mostly holepunches; strict corp firewalls fall back to relay.
- Privacy: relays are e2ee, but connection metadata (who talks to whom, when, size) is observable by the relay operator. Self-host decision 21 is for users who care.
- Troubleshooting: enable `--verbose`, check `~/.config/syncmesh/syncmesh.log` if `--log-file` used, file a GH issue with the log.
- Credits: Syncplay's protocol ideas (decision 22 risk register item: brand distinct, credit upstream).

#### iroh 1.0 upgrade (decision 3, risk register)

Pinned 0.98 today. When 1.0 ships:

- Renames to reconcile: `EndpointAddr`/`EndpointId` may revert to `NodeAddr`/`NodeId`. Crypto feature flag names (`tls-ring`) may change.
- Re-run the loopback integration suite (`crates/syncmesh-net/tests/loopback.rs`) against 1.0.
- Re-check `MeshConfig::localhost()`'s `presets::Minimal` still exists or has been renamed.
- Budget: **2 days**. Non-trivial because our code touches `EndpointAddr`, `Connection::paths()`, `Endpoint::remote_info()` — all surfaces the n0 team has been iterating on.

#### Observability (spec §5.2)

A debug-pane toggle inside the TUI: per-peer RTT sparkline (last 10 samples), signed drift in ms, control-event rate in/out. Hidden behind `Ctrl-d` so it's not in the main view. Useful during dogfooding for tuning drift tiers (decision 14) and the 1 Hz heartbeat rate (decision 5). Could move earlier if Phase 5 dogfooding exposes mystery drift — otherwise Phase 6 polish.

### Exit criterion

Verbatim from the plan: **v0.1.0 released. Single binary <15 MB per platform. curl-able install script. First external user gets through the invite flow without help.**

Semver policy per the spec: v0.x until the ticket format + config file schema + CLI surface survive a month of external use without breaking change. Then v1.0.

---

## Cross-cutting risks

Updated view of the risk register:

| Risk | Current status | Next action |
|------|----------------|-------------|
| **iroh 1.0 API churn** | realized at 0.98 pin | 2-day upgrade pass after 1.0 ships, pre-release |
| **mpv IPC latency on Windows named pipes** | Phase 1 tests pass | Bench in Phase 5 under interactive load; if p99 >5 ms, fall back to TCP loopback (mpv supports `--input-ipc-server=tcp://127.0.0.1:PORT`) |
| **Hole-punching success rate** | unknown at real scale | Phase 6 dogfooding will surface real numbers; relay is already working as fallback |
| **ratatui UX rejection by mass audience** | expected | v1 is power-user audience by design; v1.2 egui port remains contingency |
| **n0 public relay availability** | mitigated | Self-host config shipping in Phase 6 |
| **Symmetric-NAT unreachable-relay pair** | no code fix viable | Document as known issue; VPN/hotspot workaround in FAQ |
| **mpv IPC format break** | very low | Decision 18 risk register: pin mpv version in README; IPC has been stable since 2015 |
| **Syncplay brand confusion** | mitigated | Name chosen distinct; credit Syncplay explicitly in README |
| **Code-signing cost & admin overhead** | new, Phase 6 | Ship unsigned v0.1.0 with doc'd bypass; budget cert costs for v0.2 if uptake justifies |
| **Windows clipboard / TUI edge cases** | unknown | Specific manual checklist item before Phase 5 release |

---

## Suggested sequencing + cadence

The plan's original estimate was 6–8 weeks total. Phases 0–3 plus most of Phase 4 are done; what's left breaks down approximately:

1. **Week 1** — Phase 4 remainder (N>2 + address registry + 3-peer integration test).
2. **Weeks 2–3** — Phase 5 (TUI + chat + snapshot plumbing + startup UX + help overlay).
3. **Week 4** — Deliberate **dogfooding window**. Maintainer + a handful of friends use main-branch builds for real watch nights. Collect drift stats, RTT distributions, packaging papercuts, NAT failure cases. Open issues, triage what lands in v0.1 vs v0.2.
4. **Weeks 5–6** — Phase 6 (config, logging, cross-compile CI, README, install script, iroh 1.0 upgrade if available, first signed release).

Notes on cadence:

- **Do not rush Phase 5 into Phase 6.** Packaging churn on unreleased UX is wasted work — fingerprints change, shortcuts change, ticket format evolves. Phase 5 should feel *done* before cross-compile binaries get names attached.
- **Ship the dogfooding week even if nothing breaks.** Drift-tier calibration (decision 14: 100 ms / 1000 ms) and the speed=1.05 pitch-shift question (open question #1 in the spec) both need real-world signal, not synthetic tests.
- **Hold v0.1.0 until iroh 1.0 or a credibly stable 0.9x.** Re-cutting major releases to track transport churn burns trust with early users. If 1.0 is delayed indefinitely, pin 0.98 in Cargo.toml with an exact `= 0.98.x` specifier and ship anyway; the upgrade becomes a v0.2 item.
- **No feature freeze on open questions.** The four open questions at the bottom of the spec (speedup tier audibility, chat replay on join, human-memorable invites, DJ mode) stay open through Phase 6. Each gets a GH issue + label; nothing blocks v0.1.

---

*End of roadmap. Live document — edit as reality intrudes.*
