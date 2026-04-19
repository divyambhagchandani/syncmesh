# syncmesh

**A peer-to-peer [Syncplay](https://github.com/syncplay/syncplay) alternative for [mpv](https://mpv.io).** Share a ticket, everyone joins a mesh room, playback stays synced. No central server, no account, no signup.

[![CI](https://github.com/divyambhagchandani/syncmesh/actions/workflows/ci.yml/badge.svg)](https://github.com/divyambhagchandani/syncmesh/actions/workflows/ci.yml)

---

## Quickstart

```sh
# Host a room
syncmesh create --file movie.mkv

# → prints a ticket starting with `syncmesh1…`

# Friend on another machine
syncmesh join syncmesh1abc…xyz --file movie.mkv
```

Your mpv instances stay in sync on pause / seek / speed changes. Chat over the mesh with `/` in the TUI, ready-check with `r`, copy the ticket with `c`. Press `?` for the full keybind list.

## What makes syncmesh different

- **No central server.** Peers talk directly (via [iroh](https://iroh.computer) — QUIC with hole-punching and a fallback relay). The first peer is the host only until the first dial; leaving doesn't partition the mesh.
- **Mpv-native.** Spawns mpv with `--input-ipc-server` and talks to it over JSON IPC. The Syncplay protocol ideas (drift / seek thresholds) are borrowed but the wire format is our own.
- **Single static binary.** ~15 MB per platform, no runtime deps beyond an mpv you probably already have. No Python, no Twisted, no Electron.
- **Small-room by design.** Full mesh up to ~15 peers. Decision-logged in [PLAN.md](PLAN.md) if you want to know why.

## Install

### macOS / Linux — Homebrew (recommended)

```sh
brew tap divyambhagchandani/syncmesh
brew install syncmesh
```

`brew install` strips the macOS quarantine attribute, so you never see a Gatekeeper warning. Works on both Apple Silicon and Intel Macs, and on Linux (x86_64 + aarch64).

### Direct download

Get the tarball/zip from [releases](https://github.com/divyambhagchandani/syncmesh/releases) and drop `syncmesh` (or `syncmesh.exe`) into your `$PATH`. SHA256 checksums are published alongside every artifact.

- **macOS direct download** — run `xattr -r -d com.apple.quarantine ./syncmesh` once to bypass Gatekeeper (or just use Homebrew above, which handles this for you).
- **Windows** — binaries are signed via [SignPath.io](https://signpath.org) for qualifying releases; during the initial reputation-building window SmartScreen may still warn. Click "more info → run anyway" on first launch.

### From source

```sh
cargo install --git https://github.com/divyambhagchandani/syncmesh syncmesh
```

Requires Rust 1.85+. You'll also need [mpv](https://mpv.io) on your `$PATH` (or point at a binary via `mpv_binary` in the config file).

## Keybindings

| Key       | Mode   | Action                                   |
|-----------|--------|------------------------------------------|
| `r`       | Normal | Toggle your ready state                  |
| `c`       | Normal | Copy the room ticket to the clipboard    |
| `space`   | Normal | Toggle pause for the whole mesh          |
| `/`       | Normal | Enter chat mode                          |
| `tab`     | Normal | Toggle the ready-gate override           |
| `ctrl-d`  | Any    | Show / hide the observability debug pane |
| `?`       | Normal | Show help                                |
| `q`       | Normal | Quit                                     |
| `enter`   | Chat   | Send the chat message                    |
| `esc`     | Chat   | Cancel chat input                        |
| `ctrl-w`  | Chat   | Delete the last word                     |
| `ctrl-c`  | Any    | Quit (except in Help)                    |
| `PgUp/Dn` | Any    | Scroll chat scrollback                   |
| `End`     | Any    | Re-pin chat to newest, enable follow     |

## Configuration

`syncmesh` writes a commented template to your platform config dir on first run:

- Linux: `~/.config/syncmesh/config.toml`
- macOS: `~/Library/Application Support/syncmesh/config.toml`
- Windows: `%APPDATA%\syncmesh\config\config.toml`

Every setting also has a CLI flag or env var equivalent; CLI beats file beats built-in default.

```toml
# syncmesh config.toml — every line is commented; uncomment to override.
# nickname      = "divyam"
# mpv_binary    = "/usr/bin/mpv"
# mpv_spawn     = "auto"              # "auto" | "script" | "disabled"
# relay         = "https://..."       # self-hosted iroh-relay URL
# log_level     = "info"              # error|warn|info|debug|trace
# identity_path = "identity.key"
# qr            = false               # print QR code with ticket on create
```

## CLI reference

```text
syncmesh [OPTIONS] create [--file PATH]
syncmesh [OPTIONS] join <TICKET> [--file PATH]

OPTIONS (global):
  --nickname <NAME>         display name shown to peers
  --mpv-binary <PATH>       path to mpv binary (default: mpv on PATH)
  --no-mpv                  skip mpv entirely (smoke test)
  --no-spawn                connect to existing mpv via scripts/syncmesh.lua
  --no-ui                   headless, logs-only run
  --verbose                 raise stderr log filter to debug
  --log-file <PATH>         append JSON logs to PATH
  --qr                      print QR code under the ticket on create
                            (requires `--features qr` build)
```

## "I launch mpv my own way" workflow

If you'd rather start mpv yourself (or need custom flags), install the bundled Lua companion and pass `--no-spawn`:

```sh
# 1. Drop the script into your mpv scripts dir.
mkdir -p ~/.config/mpv/scripts
cp scripts/syncmesh.lua ~/.config/mpv/scripts/

# 2. Launch mpv as usual (any file, any flags).
mpv movie.mkv

# 3. From a second terminal, connect syncmesh to the script's socket.
syncmesh create --no-spawn
```

The script opens an IPC socket at a fixed path (`/tmp/syncmesh-mpv.sock` on Unix, `\\.\pipe\syncmesh-mpv` on Windows). syncmesh connects to that socket instead of spawning its own mpv.

## Running your own relay

iroh ships default relays (hosted by n0). If you want to self-host — for privacy, uptime, or policy reasons — set the `relay` key in `config.toml` to your relay URL. See [iroh's relay docs](https://iroh.computer/docs/protocols/relay) for deployment. There is no UI for this by design; it's a one-time configuration.

## FAQ

**"mpv isn't on PATH"** — set `mpv_binary` in `config.toml`, or pass `--mpv-binary /path/to/mpv`.

**"We're behind double-NAT / symmetric NAT"** — iroh's relay will handle most cases. If a direct path won't form, the relay keeps the session working. If *both* peers are on symmetric NAT and the relay is unreachable, a VPN or mobile hotspot is the last resort (documented known issue).

**"Can I use VLC / MPC-HC / Plex / a browser?"** — no. mpv-only by design; the drift algorithm depends on mpv's property change timestamps. This is [decision 16 in PLAN.md](PLAN.md) if you want the rationale.

**"Does Netflix / Disney+ / HBO work?"** — no, and never will. DRM-protected streams can't be sync-inspected from outside the player.

**"My file doesn't match my friend's"** — syncmesh warns but doesn't block. Different releases of the same movie (e.g. director's cut vs. theatrical) will drift, which is usually not what you want. Re-sync or use matching files.

**"Firewall / corp network"** — iroh does QUIC over UDP + relay fallback. Strict corporate firewalls that block all outbound UDP will fall back to the relay via HTTP(S). Fully closed networks won't work.

## Troubleshooting

Start with `--verbose --log-file ~/syncmesh.log`, reproduce the issue, then file a bug with the log attached.

Turn on the debug pane with `Ctrl-D` to see per-peer RTT and drift while the session is live. If drift is consistently non-zero, check clock skew on both machines first — syncmesh corrects drift, but it can only paper over so much.

## Privacy

All peer-to-peer traffic is end-to-end encrypted (QUIC + TLS 1.3). When the relay is used, the relay sees *that* two peers are talking and how much data, but not the contents. Self-hosting the relay fixes that if it matters. No telemetry is sent anywhere.

## Credits

The protocol ideas (drift tiers, seek thresholds, the `State` message shape) are adapted from [Syncplay](https://syncplay.pl); they've been battle-tested for 15 years and there's no reason to redesign from scratch. The network layer is [iroh](https://iroh.computer) by n0. The TUI is [ratatui](https://ratatui.rs/). Thanks to all three upstreams.

## License

Dual-licensed under MIT OR Apache-2.0.
