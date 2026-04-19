-- scripts/syncmesh.lua
--
-- "Power mode" companion for syncmesh. Load this in your own mpv instance
-- and then run `syncmesh create --no-spawn` (or `join ... --no-spawn`) from
-- a second terminal: syncmesh will connect to the IPC socket opened here
-- instead of spawning its own mpv.
--
-- Install:
--   * Drop this file into `~/.config/mpv/scripts/syncmesh.lua` (Linux/macOS)
--     or `%APPDATA%\mpv\scripts\syncmesh.lua` (Windows).
--   * Launch mpv as usual. The script will open the IPC endpoint below.
--
-- The IPC path matches `SCRIPT_IPC_PATH` in `bin/syncmesh/src/main.rs`. If
-- you need to move it, update both sides.

local utils = require 'mp.utils'

local IPC_PATH
if mp.get_property_native("platform") == "windows" then
    IPC_PATH = "\\\\.\\pipe\\syncmesh-mpv"
else
    IPC_PATH = "/tmp/syncmesh-mpv.sock"
end

-- Open the IPC socket at the predictable path. mpv's `input-ipc-server`
-- property can be set at any time; setting it here makes sure we don't
-- depend on the user passing a CLI flag. If another instance already owns
-- the path, mpv logs an error and we carry on without IPC — syncmesh will
-- fail to connect, which is the correct failure mode.
mp.set_property("input-ipc-server", IPC_PATH)
mp.msg.info(string.format("syncmesh IPC listening on %s", IPC_PATH))

-- Observe the same nine properties the syncmesh binary subscribes to when
-- it spawns mpv. The binary re-subscribes on connect, but observing here as
-- well keeps mpv's internal state consistent and surfaces a single warning
-- in mpv's log if any property name changes in a future version.
local OBSERVED = {
    "pause",
    "time-pos",
    "duration",
    "speed",
    "filename",
    "file-size",
    "path",
    "seeking",
    "eof-reached",
}
for _, name in ipairs(OBSERVED) do
    mp.observe_property(name, "native", function() end)
end
