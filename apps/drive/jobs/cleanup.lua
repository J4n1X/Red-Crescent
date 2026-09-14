-- Background housekeeping thread for Drive. It owns its own loop:
--
--   started at boot:      --thread jobs/cleanup.lua
--   or lazily on demand:  thread.spawn("drive-cleanup", "jobs/cleanup.lua")
--
-- Expiry is always enforced at access time; this thread just garbage-collects
-- dead rows and stray files. Orphans are only touched when they are at least
-- an hour old, so an upload in flight is never mistaken for garbage.

local config = require("config")

local INTERVAL_SECS = config.cleanup_interval or 300
local ARCHIVE_RETENTION_SECS = config.archive_retention or 600
local ARCHIVE_RETENTION_MINS = math.max(1, math.floor(ARCHIVE_RETENTION_SECS / 60))
local GRACE_SECS = 3600

local function schema_ready(db)
    return #db:query(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'sessions'") == 1
end

local function run_once(db)
    local now = os.time()

    local sessions = db:execute("DELETE FROM sessions WHERE expires_at <= ?", { now })
    local shares = db:execute(
        "DELETE FROM shares WHERE expires_at IS NOT NULL AND expires_at <= ?", { now })
    -- Throttling rows only matter inside their window; anything older is
    -- dead weight on every login query.
    db:execute("DELETE FROM login_attempts WHERE at < ?", { now - 86400 })

    if sessions.changes > 0 or shares.changes > 0 then
        print("cleanup: removed " .. sessions.changes .. " expired sessions, "
            .. shares.changes .. " expired shares")
    end

    -- Finished archive jobs and the files they produced. The row is dropped
    -- with the file so a stale 'ready' row can never point at a path the
    -- sweeper already took.
    local stale = db:query([[
        SELECT id, path FROM archive_jobs
         WHERE finished_at IS NOT NULL AND finished_at < ?
    ]], { now - ARCHIVE_RETENTION_SECS })
    for _, job in ipairs(stale) do
        if job.path then os.remove(job.path) end
        db:execute("DELETE FROM archive_jobs WHERE id = ?", { job.id })
    end
    if #stale > 0 then
        print("cleanup: removed " .. #stale .. " finished archive job(s)")
    end

    local files_dir = server.data_dir .. "/drive/files"

    -- Stored files with no DB row (older than the grace period).
    local listing = process.run{
        "find", files_dir, "-maxdepth", "1", "-type", "f", "-mmin", "+60",
        timeout = 120,
    }
    if listing.ok then
        local removed = 0
        for path in listing.stdout:gmatch("[^\n]+") do
            local stored_name = path:match("([^/]+)$")
            if stored_name and #db:query(
                "SELECT 1 FROM files WHERE stored_name = ?", { stored_name }) == 0 then
                os.remove(path)
                removed = removed + 1
            end
        end
        if removed > 0 then
            print("cleanup: removed " .. removed .. " orphaned stored files")
        end
    end

    -- DB rows whose stored file vanished (older than the grace period).
    for _, file in ipairs(db:query(
        "SELECT id, stored_name, name FROM files WHERE created_at <= ?",
        { now - GRACE_SECS })) do
        local handle = io.open(files_dir .. "/" .. file.stored_name, "rb")
        if handle then
            handle:close()
        else
            db:execute("DELETE FROM files WHERE id = ?", { file.id })
            print("cleanup: dropped row for missing file '" .. file.name .. "'")
        end
    end

    -- Stray spool files from crashed requests.
    process.run{
        "find", server.data_dir .. "/.spool",
        "-maxdepth", "1", "-type", "f", "-mmin", "+60", "-delete",
        timeout = 120, capture = false,
    }

    -- Folder-download archives (and any staging left by a crashed export).
    -- They are streamed to the client immediately after being built and can
    -- be gigabytes each, so they get a much shorter grace period than the
    -- upload spool — long enough to outlive a slow download, short enough
    -- that a few big exports cannot sit on the disk for an hour.
    process.run{
        "find", server.data_dir .. "/drive/tmp",
        "-mindepth", "1", "-maxdepth", "1", "-mmin", "+" .. ARCHIVE_RETENTION_MINS,
        "-exec", "rm", "-rf", "--", "{}", "+",
        timeout = 300, capture = false,
    }
end

local db = sqlite.open("drive/drive.db")
while true do
    if schema_ready(db) then
        local ok, err = pcall(run_once, db)
        if not ok then
            log.warn("cleanup: iteration failed: " .. tostring(err))
        end
    else
        log.info("cleanup: schema not initialized yet")
    end
    sleep(INTERVAL_SECS)
end
