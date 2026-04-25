# Getting started with syncmesh (Windows)

This walks you from the freshly-built `syncmesh.exe` to a working sync session in about five minutes.

## 1. Prerequisites

- **Windows 10 or 11** (x86_64).
- **mpv** on your `PATH`. Easiest install:
  ```powershell
  winget install mpv
  ```
  Verify with `mpv --version`. If `winget` isn't available, grab a build from [mpv.io](https://mpv.io/installation/) and add its folder to `PATH`.
- **A media file** that you and your friend both already have locally. syncmesh syncs playback — it does not stream the file.

## 2. Put the binary somewhere sensible

The release build is at `target\release\syncmesh.exe` (~15 MB, single file, no DLLs to ship).

Pick one of:

```powershell
# Option A: drop it next to mpv / on PATH
copy target\release\syncmesh.exe %USERPROFILE%\bin\

# Option B: just run it from where it is
.\target\release\syncmesh.exe --version
```

You should see `syncmesh 0.1.0`.

## 3. Host a room

```powershell
syncmesh create --file "C:\Videos\movie.mkv"
```

You'll see:

1. A **ticket** starting with `syncmesh1…`. Copy it (or press `c` in the TUI to copy to clipboard).
2. The **TUI** with a peer list, chat pane, and status line.
3. **mpv** spawning automatically with your file paused at frame 0.

Send the ticket to your friend through any channel.

> **Windows SmartScreen**: on first run you may see a "Windows protected your PC" warning. Click **More info → Run anyway**. Releases will be signed once they hit the SignPath reputation threshold.

> **Firewall prompt**: allow `syncmesh.exe` on private networks. UDP outbound is what matters; if you decline, peers will fall back to the iroh relay automatically.

## 4. Friend joins

On the other machine (with the same file already on disk):

```powershell
syncmesh join syncmesh1abc…xyz --file "D:\Movies\movie.mkv"
```

Once both peers are connected, the peer list shows them. Each peer toggles ready with `r`. When everyone's ready, playback unpauses for the whole mesh.

## 5. Driving the session

| Key      | What it does                          |
|----------|---------------------------------------|
| `r`      | Toggle your ready state               |
| `space`  | Pause / unpause the whole mesh        |
| `c`      | Copy the ticket to clipboard          |
| `/`      | Enter chat                            |
| `tab`    | Override the ready-gate (start anyway)|
| `Ctrl-D` | Show RTT / drift debug pane           |
| `?`      | Full keybind help                     |
| `q`      | Quit                                  |

Pause, seek, and speed changes propagate automatically. If your clock drifts or the network hiccups, syncmesh nudges mpv back into alignment — you'll see it on the debug pane.

## 6. Config file (optional)

First run writes a commented template to:

```
%APPDATA%\syncmesh\config\config.toml
```

Common edits:

```toml
nickname   = "divyam"
mpv_binary = "C:\\Program Files\\mpv\\mpv.exe"   # if mpv isn't on PATH
log_level  = "debug"                              # for bug reports
```

CLI flags override the file; the file overrides built-in defaults.

## 7. Troubleshooting

**`mpv` not found** — pass `--mpv-binary "C:\path\to\mpv.exe"` or set `mpv_binary` in the config.

**Nothing happens after `join`** — confirm you and the host can both reach the internet on UDP. Add `--verbose --log-file %TEMP%\syncmesh.log`, reproduce, and inspect the log. iroh's relay should fall back automatically over HTTPS even on locked-down networks.

**Drift won't settle** — check system clocks on both ends. syncmesh corrects up to a couple seconds of skew; beyond that, fix the clocks (Windows Time service, or `w32tm /resync`).

**File mismatch warning** — different releases of the same title (director's cut vs theatrical, different rips) will drift. Use matching files.

## 8. Next steps

- [README.md](README.md) — full feature list, design rationale, FAQ.
- [PLAN.md](PLAN.md) — architectural decisions and why.
- `syncmesh --help` and `syncmesh create --help` for every flag.

To rebuild after a code change:

```powershell
cargo build --release --bin syncmesh
```

The output lands at `target\release\syncmesh.exe`. That's the entire artifact — no installer, no runtime, ship it as-is.
