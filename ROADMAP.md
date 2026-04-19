# syncmesh — forward roadmap

Companion to [PLAN.md](PLAN.md). Where PLAN.md is a status board, this doc is the detailed forward-looking plan for everything left. Decisions from the original spec are **frozen** unless called out explicitly; when one constrains an implementation choice, it's cited inline as "(decision N)" per [PLAN.md Decision index](PLAN.md#decision-index).

---

## Table of contents

- [Where we are](#where-we-are)
- [What's left — short answer](#whats-left--short-answer)
- [Dogfooding window (between Phase 5 and Phase 6)](#dogfooding-window-between-phase-5-and-phase-6)
- [Phase 6 — polish, packaging, release](#phase-6--polish-packaging-release)
- [Phase 5 polish items deferred to Phase 6](#phase-5-polish-items-deferred-to-phase-6)
- [Test-coverage gaps worth closing](#test-coverage-gaps-worth-closing)
- [Cross-cutting risks](#cross-cutting-risks)
- [Suggested sequencing + cadence](#suggested-sequencing--cadence)

---

## Where we are

As of [commit `062f449`](https://github.com/divyambhagchandani/syncmesh/commit/062f449) on branch `docs/roadmap-phase6-plan`:

- ✅ **Phase 0–3** — scaffolding, mpv integration, two-peer mesh, sync state machine. Shipped.
- ✅ **Phase 4** — end-to-end wiring in the binary **including N>2 full-mesh dialing** (decisions 4 and 8). `PresenceEvent::PeerList` carries opaque postcard-encoded `EndpointAddr` bytes; new `PresenceEvent::AddrAnnounce` for on-connect broadcasts; `AddrRegistry` in the bin layer plumbs the address flow. Three-peer integration test ([`bin/syncmesh/tests/mesh_3peer.rs`](bin/syncmesh/tests/mesh_3peer.rs)) proves a peer joining via a second-hop ticket establishes a direct link to the original host and exchanges control frames without transiting the middleman.
- ✅ **Phase 5** — TUI + chat. `RoomState::snapshot()` projects into an immutable `RoomSnapshot`; App publishes it on a `tokio::sync::watch` channel after every dispatch; a `ui/` task reads at its own cadence, translates crossterm key events into `UiEvent`s, and pushes them back through an mpsc. Clipboard copy behind the default `clipboard` feature. `--no-ui` flag for headless/CI runs. Three Phase-5-polish items landed on top: splash screen on no-subcommand, PgUp/PgDn chat scrollback with follow-mode auto-reset, and a hand-rolled `App::Debug`. 55 bin-crate tests (was 48), all passing.
- 🧮 **Test count**: 217 passing, clippy-clean with `-D warnings --all-features`.

What's left: Phase 6 (polish, packaging, release) plus a deliberate dogfooding week to calibrate the open questions from the original spec, plus a short list of deferred polish items and test-coverage gaps.

---

## What's left — short answer

1. **Dogfooding week** (~5 days) — real watch nights to answer four spec-open questions and surface packaging papercuts before they become user-visible defects. Zero code change assumed; the payload is issue tickets + a list of calibration tweaks.
2. **Phase 6** (~7–10 days focused work) — config file, logging, self-hosted relay plumbing, QR-code invites, Lua power mode, cross-compile CI for five targets, signing strategy, distribution channels, README, iroh 1.0 upgrade pass, observability debug pane. Exit criterion: **v0.1.0 released; single binary <15 MB per platform; first external user completes the invite flow without help.**
3. **Phase 5 deferred polish** (~0.5 day remaining) — splash screen ✅, chat scrollback keybinds ✅, `App` Debug impl ✅ all shipped in [`062f449`](https://github.com/divyambhagchandani/syncmesh/commit/062f449). IME / non-ASCII chat input hardening and terminal-resize manual checklist remain; neither blocks v0.1.
4. **Test-coverage gaps** (~0.5 day remaining) — `AddrAnnounce` policy guard ✅ and chat-ring eviction ✅ now covered (the latter was already green; plan line was stale). The higher-level `UiEvent` → `App` → `Output` integration test and the `RoomState` → snapshot → UI smoke are still open.

Everything in this list is scoped in detail below.

---

## Dogfooding window (between Phase 5 and Phase 6)

The original plan calls out a deliberate dogfooding week before cross-compile CI and signing work begins. Packaging churn on unreleased UX wastes effort — fingerprints, shortcuts, ticket format, and chat semantics can still evolve based on real-world signal. Holding Phase 6 until dogfooding validates the UX keeps the first release honest.

### Open questions the spec left dangling

These were flagged in the original [PLAN.md open questions](PLAN.md#open-questions-for-phase-34-dogfooding). They can only be answered with real users on real networks.

1. **Does the `speed=1.05` speedup tier (decision 14) cause audible pitch shift worse than the 200-ms-ahead it corrects?** Cheap A/B: run a side-by-side session where one peer has drift tiers at their default and another at `DRIFT_SLOWDOWN_MS = 200` (i.e. relax the speedup trigger). Judgement call based on listener feedback.
2. **Should late joiners see room chat history?** Today [`RoomState::chat_ring`](crates/syncmesh-core/src/state.rs) keeps a 200-msg ring but never replays it on join. Decision scope: either (a) send a snapshot on `PeerConnected` (one-time cost, bounded ≤200 × ~100B = 20 KB), or (b) leave history local and document that chat is ephemeral. Picking (a) means a small `PresenceEvent::ChatBacklog` variant plus dedup-by-`(origin, origin_ts_ms)` on the receiver.
3. **Hole-punching success rate on real internet** (risk register item, currently "open"). iroh's relay is a fallback — but how often does direct-path actually succeed vs. stay on relay? Add light telemetry: log `Endpoint::remote_info(remote_id)` paths on connect, grep logs after a session, tabulate.
4. **mpv IPC latency on Windows named pipes under interactive load** (risk register). Benchmark during a real session with frequent scrubs and confirm p99 <5 ms; if not, switch to `--input-ipc-server=tcp://127.0.0.1:PORT` as the fallback.

### What dogfooding produces

- A GitHub issue per open question with the collected data and a recommendation.
- Drift-tier calibration confirmed or adjusted in [`syncmesh-core::drift`](crates/syncmesh-core/src/drift.rs) if the tiers turn out to be audibly wrong.
- A punch-list of UX papercuts for the Phase 6 polish pass (stuff like "the ticket line wraps in Windows Terminal at 80 cols", "the 'peers' pane overflows with 8 peers", "Ctrl-C during chat input leaves a dangling line").

### Exit criterion

Three real watch nights with at least two distinct machine setups (ideally mixed OS). Drift stays under 100 ms in steady state. Zero crashes. The four open questions have concrete answers (even if some of those answers are "defer to v1.1"). Anyone in the dogfooding group can cut a release-candidate build from `main` and run it without reading docs.

---

## Phase 6 — polish, packaging, release

### Spec decisions honored

- **Decision 3** — iroh transport. We're pinned at 0.98; **1.0 upgrade** lives in this phase.
- **Decision 16** — app spawns mpv by default; Lua-script power mode is shipped for users who launch mpv their own way.
- **Decision 20** — QR-code in-terminal rendering (originally tagged "v1.1"; folded into Phase 6 since the release vehicle exists).
- **Decision 21** — n0 relays are the default; self-hosted iroh-relay is a config key. Relay status is not surfaced in the UI.

### Work items

#### 1. Config file (~1 day)

- Location: `directories::ProjectDirs::from("", "", "syncmesh").config_dir()/config.toml`.
- Schema (`serde`-deserialized into a `Config` struct):
  ```toml
  nickname      = "divyam"                        # default peer nickname
  mpv_binary    = "/usr/bin/mpv"                  # override mpv lookup
  mpv_spawn     = "auto"                          # "auto" | "script" | "disabled"
  override_mode = false                           # default state of ready-gate override
  relay         = "https://relay.example.com"     # optional — empty = use n0 defaults
  log_level     = "info"                          # "error"|"warn"|"info"|"debug"|"trace"
  identity_path = "identity.key"                  # rel to config_dir; absolute also allowed
  ```
- Precedence: CLI flag > config file > built-in default. CLI exposes only `nickname`, `mpv_binary`, `log_level`; the rest live in the file.
- On first run, if no config file exists, write a **commented-out** template to the config dir so the user discovers it without needing to read docs.
- Deps to add: `toml = "0.8"`, `serde` (already present).
- Files touched: [`bin/syncmesh/src/config.rs`](bin/syncmesh/src/config.rs), [`main.rs`](bin/syncmesh/src/main.rs) load path.

#### 2. Logging CLI flags (~0.5 day)

- `--verbose` → raises default filter to `debug`.
- `--log-file <path>` → structured JSON logs via `tracing-appender` rolling daily. Useful for bug reports.
- Env var `SYNCMESH_LOG` stays (already works).
- Default stderr filter stays `info,syncmesh=debug`.
- Deps: `tracing-appender = "0.2"`.
- Files touched: [`bin/syncmesh/src/cli.rs`](bin/syncmesh/src/cli.rs), [`main.rs::init_tracing`](bin/syncmesh/src/main.rs).

#### 3. Self-hosted relay plumbing (~0.5 day)

- Extend [`MeshConfig`](crates/syncmesh-net/src/mesh.rs) with `relay_override: Option<RelayUrl>`.
- Plumb through `Endpoint::builder(...).relay_conf(...)`; re-verify the shape against iroh 0.98 docs before coding (may rename in 1.0).
- No UI surface (decision 21: hidden from UI). Just a config-file line.
- README section: "Running a self-hosted iroh-relay"; reference iroh's own deployment docs.

#### 4. QR-code invites (~0.5 day)

- `qrcode = "0.14"`, optional dep behind a `qr` feature flag.
- On `create`, print the text ticket first, then the QR underneath rendered with Unicode half-blocks (each QR pixel = half a terminal row). Gate with `--qr` or `qr = true` in config so users on terminals that mangle Unicode aren't confused.
- Future (v1.2): mobile companion that scans the QR and hands off to a desktop mpv/syncmesh — explicit non-goal here.

#### 5. Lua-script power mode (~1 day)

- Small Lua file (~40 lines) shipped in `scripts/syncmesh.lua`:
  - Uses `mp.utils.subprocess` or the builtin `mpv-ipc` to expose an IPC socket at a fixed, predictable path.
  - Sets the nine observed properties (decision 18) so syncmesh sees events even though it didn't spawn mpv.
- Binary support: `mpv_spawn = "script"` in config or `--no-spawn` CLI flag. When enabled, skip `spawn()` and just `connect_transport(ipc_path)` against the script-side socket.
- Files touched: `scripts/syncmesh.lua` (new), [`syncmesh-player::process`](crates/syncmesh-player/src/process.rs) (add "connect only" entry point), [`main.rs`](bin/syncmesh/src/main.rs).
- README section: "I launch mpv my own way" workflow.

#### 6. Cross-compile matrix (~1 day + CI debugging)

| Target                         | Host    | Tool              |
|--------------------------------|---------|-------------------|
| `x86_64-unknown-linux-musl`    | Linux   | `cargo-zigbuild`  |
| `aarch64-unknown-linux-musl`   | Linux   | `cargo-zigbuild`  |
| `x86_64-apple-darwin`          | macOS   | native            |
| `aarch64-apple-darwin`         | macOS   | native            |
| `x86_64-pc-windows-msvc`       | Windows | native            |

Static musl targets keep the binary portable across distros. macOS universal2 is nice-to-have; two separate arch binaries are acceptable for v0.1.0.

#### 7. Signing + notarization

- **macOS**: requires an Apple Developer ID (~$99/yr). If we pay: `codesign --sign "Developer ID Application: ..."` → zip → `xcrun notarytool submit` → `xcrun stapler staple`. If we don't pay: README ships explicit `xattr -r -d com.apple.quarantine ./syncmesh` instructions and a brief Gatekeeper explainer.
- **Windows**: OV cert is ~$400+/yr; winget catalog requires signed binaries. For v0.1.0, ship unsigned; publish SHA256 on releases; direct-download still works with a SmartScreen warning. Defer winget submission until a cert is in place.
- **Linux**: no signing. Publish SHA256 checksums + an ASCII-armored GPG signature from maintainer's key, advertised in README.

Decision to make in the dogfooding window: **pay for macOS cert now or after initial traction?** Recommendation: after traction. Ship v0.1.0 with documented bypass.

#### 8. Release CI (~1.5 days)

- Trigger on tag `v*`.
- Matrix job builds all five targets.
- macOS signs + notarizes if `APPLE_ID` / `APPLE_APP_PASSWORD` / `APPLE_TEAM_ID` secrets are present; Windows signs if `WINDOWS_CERT_BASE64` / `WINDOWS_CERT_PASSWORD` are set; otherwise skip with a warning (so the CI pipeline doesn't block on absent secrets during v0.1).
- Upload artifacts + `SHA256SUMS` to the GitHub Release.
- Post-hook updates Homebrew tap formula (bumps version + sha256) by pushing to a tap repo.
- Post-hook updates winget manifest via `winget-create` (optional until signing is in place).
- Files touched: `.github/workflows/release.yml` (new).

#### 9. Distribution channels (~1 day to set up, some ongoing)

- **GitHub Releases** — canonical source. Raw tarballs, sha256, optional `.sig`.
- **Homebrew tap** — separate repo; formula bumped by release CI. `brew tap <user>/syncmesh && brew install syncmesh`.
- **winget** — once signing is sorted. Adds Windows one-liner install.
- **AUR** — `syncmesh-bin` PKGBUILD pulling the musl static binary; community-maintainable.
- **Install script** — `curl -sSL https://syncmesh.sh | sh` that detects OS/arch, downloads, verifies checksum, drops into `~/.local/bin`. Ship last; requires a domain.

#### 10. README (~1 day)

- Screencast at the top (asciinema `.cast` or animated WebM).
- Quickstart: install → `syncmesh create --file movie.mkv` → share ticket → friend runs `syncmesh join <ticket> --file movie.mkv`.
- FAQ:
  - "mpv isn't on PATH" — config file example.
  - "We're behind double-NAT / symmetric NAT" — iroh handles it via relay; VPN or mobile hotspot as last resort.
  - "Can I use VLC?" — no (decision intentional).
  - "Does Netflix/etc work?" — no (decision intentional).
  - "Files don't match" — mismatch is non-blocking (decision 11); ignore warning if intentional (different releases).
  - "Firewall / router ports" — iroh mostly holepunches; strict corp firewalls fall back to relay.
- Privacy: relays are e2ee, but connection metadata (who talks to whom, when, size) is observable by the relay operator. Self-host (decision 21) is for users who care.
- Troubleshooting: enable `--verbose`, check `~/.config/syncmesh/syncmesh.log` if `--log-file` used, file a GH issue with the log.
- Credits: Syncplay's protocol ideas (risk register item: brand distinct, credit upstream).

#### 11. iroh 1.0 upgrade (~2 days, when 1.0 lands)

Pinned 0.98 today. When 1.0 ships:

- Renames to reconcile: `EndpointAddr`/`EndpointId` may revert to `NodeAddr`/`NodeId`. Crypto feature flag names (`tls-ring`) may change.
- Re-run the loopback integration suite (`crates/syncmesh-net/tests/loopback.rs`) against 1.0.
- Re-check `MeshConfig::localhost()`'s `presets::Minimal` still exists or has been renamed.
- Non-trivial: our code touches `EndpointAddr`, `Connection::paths()`, `Endpoint::remote_info()` — all surfaces n0 has been iterating on.
- If 1.0 is delayed indefinitely, pin `= 0.98.x` in Cargo.toml and ship v0.1 anyway; upgrade becomes a v0.2 item.

#### 12. Observability debug pane (~0.5 day)

- Ctrl-d toggle inside the TUI: per-peer RTT sparkline (last 10 samples), signed drift in ms, control-event rate in/out.
- Hidden behind `Ctrl-d` so it's not in the main view. Useful during dogfooding for tuning drift tiers (decision 14) and 1 Hz heartbeat rate (decision 5).
- Could move earlier if Phase 5 dogfooding exposes mystery drift — otherwise Phase 6.

### Exit criterion

Verbatim from the plan: **v0.1.0 released. Single binary <15 MB per platform. curl-able install script. First external user gets through the invite flow without help.**

Semver policy: **v0.x** until the ticket format + config file schema + CLI surface survive a month of external use without breaking change. Then v1.0.

---

## Phase 5 polish items deferred to Phase 6

The Phase 5 implementation shipped the core TUI and chat flow but flagged a few things as non-blocking. Items 1–3 shipped in [commit `062f449`](https://github.com/divyambhagchandani/syncmesh/commit/062f449); 4 and 5 remain.

1. ✅ **Splash screen** — `syncmesh` with no subcommand now prints a short usage splash (`create` / `join <TICKET>`) and exits 0 instead of clap-erroring. `Cli::command` became `Option<Command>`; the splash lives in `main::print_splash`.
2. ✅ **Chat scrollback keybinds** — PgUp / PgDown / End are wired in both Normal and Chat modes. `chat_scroll` was reframed from "lines from top" to "lines scrolled up from bottom" and a new `chat_follow` flag implements the follow-mode auto-reset: new messages stay pinned to the bottom by default, PgUp breaks follow, End (or scrolling all the way down) re-arms it. Step size is 5 lines.
3. ✅ **`App` Debug impl** — hand-rolled, skips `MeshEndpoint` / `MpvHandle` / `PeerLink`-bearing fields and the mpsc/watch channel ends; surfaces `mpv_spawned: bool` + `peer_count` instead. Uses `finish_non_exhaustive`.
4. ⬜ **IME / non-ASCII chat input** (~0.5 day, maybe more) — today `KeyCode::Char(c)` just appends; no IME composition. Goal for v0.1 is "doesn't crash". If it does crash on Japanese/Korean input during dogfooding, prioritize accordingly.
5. ⬜ **Terminal resize robustness** — ratatui handles resize transparently, but should be a manual checklist item before release (80×24 → 200×60, min/max bounds).

---

## Test-coverage gaps worth closing

Mostly insurance against regressions during Phase 6 churn.

1. ⬜ **End-to-end `UiEvent` → `App` → `Output` test** (~0.5 day) — [`App`](bin/syncmesh/src/app.rs) has no direct test coverage; the 3-peer integration test covers the wire protocol, the TUI tests cover the render tree, but no test exercises "press `r`, assert `PresenceEvent::Ready` broadcast". Wire an in-process App in a test harness: feed it a synthetic `UiEvent::ToggleReady`, observe the per-peer writer mpsc. Would also catch bugs in the snapshot publishing after dispatch. *Deferred: requires a real-`MeshEndpoint` test harness; not worth building until another item needs one.*
2. ✅ **`AddrAnnounce` round-trip** — frame-level encode/decode was already covered by `presence_variants_all_roundtrip` in `crates/syncmesh-core/src/protocol.rs`. The registry-level policy (non-empty + non-self) was extracted from `App::on_peer_frame` into `AddrRegistry::apply_announce` with three unit tests covering happy path, self-skip, and empty-bytes rejection. [`062f449`](https://github.com/divyambhagchandani/syncmesh/commit/062f449).
3. ⬜ **Snapshot → UI render smoke** (~0.2 day) — every UI render test today uses a hand-constructed `RoomSnapshot`. One test should build a real `RoomState`, drive some inputs, call `.snapshot()`, and render. Catches the "we forgot to add field X to snapshot()" class of bug.
4. ✅ **Chat-ring eviction** — already covered by `chat_ring_evicts_oldest_beyond_capacity` in `state.rs:1227` (adds 210 msgs, asserts count == 200, verifies the first 10 are gone). The original plan line was out of date.
5. ⬜ **N>2 disconnect / re-dial test** — would validate the non-goal from the Phase 4 roadmap ("NAT rebinding mid-session"). Not a blocker but worth a spec line.

---

## Cross-cutting risks

| Risk | Current status | Next action |
|------|----------------|-------------|
| **iroh 1.0 API churn** | realized at 0.98 pin | 2-day upgrade pass after 1.0 ships, pre-release |
| **mpv IPC latency on Windows named pipes** | Phase 1 tests pass | Bench during dogfooding under interactive load; if p99 >5 ms, fall back to TCP loopback |
| **Hole-punching success rate** | unknown at real scale | Dogfooding week will surface real numbers; relay is already working as fallback |
| **ratatui UX rejection by mass audience** | expected | v1 is power-user audience by design; v1.2 egui port remains contingency |
| **n0 public relay availability** | mitigated | Self-host config shipping in Phase 6 |
| **Symmetric-NAT unreachable-relay pair** | no code fix viable | Document as known issue; VPN/hotspot workaround in FAQ |
| **mpv IPC format break** | very low | IPC has been stable since 2015; pin mpv version in README |
| **Syncplay brand confusion** | mitigated | Name distinct; credit Syncplay explicitly in README |
| **Code-signing cost & admin overhead** | Phase 6 decision | Ship unsigned v0.1.0 with doc'd bypass; budget cert costs for v0.2 if uptake justifies |
| **Windows clipboard / TUI edge cases** | unknown | Manual checklist item before release |
| **Chat scroll + auto-follow UX** | known gap | Fix in Phase 5 polish (deferred); easy |

---

## Suggested sequencing + cadence

Original spec estimate was 6–8 weeks total for Phases 0–6. With Phases 0–5 shipped, the remaining work fits in **~2.5 weeks** of focused effort:

| Week | Scope |
|------|-------|
| 1 | **Dogfooding window** — three watch nights, collect drift/RTT/hole-punch data, triage open questions into GH issues, close Phase 5 polish items that surface, pick the signing path. |
| 2 | **Phase 6 foundation** — config file, logging flags, self-hosted relay plumbing, Lua script, QR-code invites, splash screen, chat scroll polish, observability pane, the small test-coverage fills. |
| 3 | **Phase 6 release** — cross-compile matrix, CI release workflow, README with screencast, distribution channels, v0.1.0 tag. iroh 1.0 upgrade if available; otherwise defer. |

### Cadence guardrails

- **Do not skip the dogfooding week.** Packaging churn on unreleased UX wastes effort. Let reality intrude before binaries get version numbers.
- **Ship v0.1.0 signed-or-unsigned according to budget.** Unsigned + documented bypass is fine for initial release; chasing certs doesn't block the first users.
- **Hold v0.1.0 until iroh 1.0 or a credibly stable 0.9x.** Re-cutting major releases to track transport churn burns trust. If 1.0 delays past week 3, pin exactly and ship.
- **Open questions stay open through Phase 6.** The four spec open questions each get a GH issue + label; nothing blocks v0.1.

---

*End of roadmap. Live document — edit as reality intrudes.*
