-- Folder downloads, built by shelling out to whatever archiver the host has.
--
-- Drive stores file bytes flat under random names, so an archive is produced
-- by staging a tree of hard links (real names, real structure, no data copy)
-- and running the archiver over it. Everything user-controlled reaches the
-- shell single-quoted, and every command uses `--` so a name starting with
-- "-" can never be read as an option.
local filesm = require("lib.files")
local config = require("config")

local M = {}

M.MAX_FILES = config.archive_max_files or 20000
M.MAX_BYTES = config.archive_max_bytes or 8 * 1024 * 1024 * 1024
M.SUBPROCESS_TIMEOUT = 600 -- seconds; enforced by process.run itself

-- Present if the binary can be started at all; a non-zero exit still means
-- it exists, which is the only question here.
local function have(tool)
    return process.run{ tool, "--version", capture = false, timeout = 5 }.started
end

M.tmp_dir = server.data_dir .. "/drive/tmp"

-- Discovery: the archive feature only exists if the host can archive.
-- Cached for the life of this Lua instance (one request or thread).
local detected, detected_done = nil, false
function M.detect()
    if not detected_done then
        detected_done = true
        if have("zip") then
            detected = { name = "zip", ext = "zip", content_type = "application/zip" }
        elseif have("tar") then
            detected = { name = "tar", ext = "tar.gz", content_type = "application/gzip" }
        end
    end
    return detected
end

-- Walks the folder tree, returning the directories to create and the files to
-- link, or nil + message when the export would be too large.
local function collect(db, user, folder)
    local dirs, files, bytes = {}, {}, 0
    local queue = { { folder = folder, prefix = "" } }

    while #queue > 0 do
        local node = table.remove(queue, 1)
        local subfolders, contents = filesm.list(db, user, node.folder)

        for _, file in ipairs(contents) do
            bytes = bytes + (file.size or 0)
            if #files >= M.MAX_FILES then
                return nil, "this folder holds more than " .. M.MAX_FILES .. " files"
            end
            if bytes > M.MAX_BYTES then
                return nil, "this folder is larger than "
                    .. math.floor(M.MAX_BYTES / 1024 / 1024) .. " MiB"
            end
            table.insert(files, {
                src = filesm.disk_path(file),
                rel = node.prefix .. file.name,
            })
        end

        for _, sub in ipairs(subfolders) do
            local rel = node.prefix .. sub.name
            table.insert(dirs, rel) -- listed even when empty, so it survives
            table.insert(queue, { folder = sub, prefix = rel .. "/" })
        end
    end

    if #files == 0 and #dirs == 0 then
        return nil, "this folder is empty"
    end
    return { dirs = dirs, files = files, bytes = bytes }
end

-- Stages the tree with fs calls rather than a generated shell script: no
-- filename ever becomes shell syntax, and there is no process per file.
-- Hard links keep it O(1) in data volume; fs.link copies when it must.
local function stage(staging, plan)
    local ok, err = pcall(function()
        fs.mkdir(staging)
        for _, rel in ipairs(plan.dirs) do
            fs.mkdir(staging .. "/" .. rel)
        end
        for _, file in ipairs(plan.files) do
            -- A source cleaned up mid-export must not abort the archive.
            local handle = io.open(file.src, "rb")
            if handle then
                handle:close()
                fs.link(file.src, staging .. "/" .. file.rel)
            end
        end
    end)
    if not ok then
        log.warn("drive: staging failed: " .. tostring(err))
    end
    return ok
end

-- Builds an archive of `folder` (nil = the whole drive).
-- Returns { path=, filename=, content_type= }, or nil + message.
function M.build(db, user, folder)
    local tool = M.detect()
    if not tool then
        return nil, "no archiver is installed on the server"
    end

    local plan, err = collect(db, user, folder)
    if not plan then return nil, err end

    if not pcall(fs.mkdir, M.tmp_dir) then
        return nil, "could not create the temporary directory"
    end

    local token = crypto.random_token(16)
    local staging = M.tmp_dir .. "/stage-" .. token
    local out = M.tmp_dir .. "/" .. token .. "." .. tool.ext
    local function cleanup()
        process.run{ "rm", "-rf", "--", staging, capture = false, timeout = 120 }
    end

    if not stage(staging, plan) then
        cleanup()
        return nil, "could not prepare the archive"
    end

    -- The archiver runs inside the staging directory, so only server-generated
    -- paths reach it at all.
    local result
    if tool.name == "zip" then
        -- -1 (fastest) on purpose: measured on 1.2 GiB of real game data,
        -- level 1 took 24s for 950 MiB against 30s for 945 MiB at the
        -- default. Compressible content still shrinks fine at level 1.
        result = process.run{
            "zip", "-rqX", "-1", out, ".",
            cwd = staging, timeout = M.SUBPROCESS_TIMEOUT, capture = false,
        }
    else
        result = process.run{
            "tar", "-czf", out, "-C", staging, ".",
            timeout = M.SUBPROCESS_TIMEOUT, capture = false,
        }
    end

    local ok = result.ok
    cleanup()
    if not ok then
        os.remove(out)
        return nil, "the archiver failed (the folder may be too large)"
    end

    local base = folder and folder.name or "drive"
    return {
        path = out,
        filename = base .. "." .. tool.ext,
        content_type = tool.content_type,
    }
end


-- === job queue =========================================================
--
-- A request only enqueues; jobs/archive.lua builds. The queue is a table
-- rather than in-memory state because it must survive a restart.

-- Age backstop; the signature below is the real guard. 0 disables it.
M.REUSE_SECS = config.archive_reuse_secs or 600

-- A fingerprint of everything that ends up in the archive: every file's id,
-- placement, name and size, and every folder's placement and name. Folders
-- because an empty one is still archived; names because a rename changes an
-- entry's path and nothing else. Derived from the data rather than maintained
-- by hand, so no mutation site can forget to invalidate it.
function M.signature(db, user, folder)
    local root = folder and folder.id or sqlite.NULL
    local row = db:query([[
        WITH RECURSIVE subtree(id, parent_id, name) AS (
            SELECT id, parent_id, name FROM folders
             WHERE owner_id = ? AND ((? IS NULL AND parent_id IS NULL) OR id = ?)
            UNION ALL
            SELECT f.id, f.parent_id, f.name FROM folders f
              JOIN subtree s ON f.parent_id = s.id
             WHERE f.owner_id = ?
        )
        SELECT
          (SELECT COALESCE(group_concat(
                    id || ':' || COALESCE(parent_id, 0) || ':' || name,
                    char(10) ORDER BY id), '') FROM subtree) AS folders,
          (SELECT COALESCE(group_concat(
                    f.id || ':' || COALESCE(f.folder_id, 0) || ':' || f.stored_name
                      || ':' || f.size || ':' || f.name,
                    char(10) ORDER BY f.id), '')
             FROM files f
            WHERE f.owner_id = ?
              AND (f.folder_id IN (SELECT id FROM subtree)
                   OR (? IS NULL AND f.folder_id IS NULL))) AS files
    ]], { user.id, root, root, user.id, user.id, root })[1]

    return crypto.sha256((row.folders or "") .. "\n--\n" .. (row.files or ""))
end

local function folder_key(folder)
    -- nil is the whole drive; SQLite's IS handles that where `=` matches nothing.
    return folder and folder.id or sqlite.NULL
end

-- A job this request can ride on: in flight for this exact tree, or finished
-- and still usable. The signature is matched in SQL, so a job building a tree
-- that has since changed is not a candidate.
local function reusable(db, user, folder, signature)
    local row = db:query([[
        SELECT * FROM archive_jobs
         WHERE owner_id = ? AND folder_id IS ? AND signature = ?
           AND status IN ('queued', 'building', 'ready')
         ORDER BY id DESC LIMIT 1
    ]], { user.id, folder_key(folder), signature })[1]
    if not row then return nil end
    if row.status ~= "ready" then return row end
    if M.REUSE_SECS > 0 and os.time() - (row.finished_at or 0) > M.REUSE_SECS then
        return nil
    end
    -- A ready row whose file the sweeper already took is worse than no row.
    local handle = row.path and io.open(row.path, "rb")
    if not handle then return nil end
    handle:close()
    return row
end

-- How many exports may run at once; each is a zip process working through
-- gigabytes, so "background" must not mean "unbounded".
M.MAX_CONCURRENT = config.archive_max_concurrent or 2

function M.thread_name(id)
    return "archive-" .. tostring(id)
end

-- Counted from live threads, not the status column, so a row left behind by
-- a restart does not hold a slot forever.
function M.active_count(db)
    local n = 0
    for _, row in ipairs(db:query("SELECT id FROM archive_jobs WHERE status = 'building'")) do
        if thread.running(M.thread_name(row.id)) then n = n + 1 end
    end
    return n
end

-- Reported to the page so a queued download can say what it is waiting for.
function M.queue_stats(db, exclude_id)
    local waiting = db:query(
        "SELECT COUNT(*) AS n FROM archive_jobs WHERE status = 'queued' AND id <> ?",
        { exclude_id or -1 })[1].n
    return M.active_count(db), waiting
end

-- Makes sure this job has a task behind it, if there is room to run one.
-- Called on every poll, which is what makes it self-healing: a task lost to a
-- restart is noticed here and started again.
function M.ensure_running(db, job)
    if job.status ~= "queued" and job.status ~= "building" then return job end

    local name = M.thread_name(job.id)
    if thread.running(name) then return job end

    -- 'building' with no thread means the task is gone; nothing else moves it.
    if job.status == "building" then
        db:execute(
            "UPDATE archive_jobs SET status = 'queued' WHERE id = ? AND status = 'building'",
            { job.id })
        job.status = "queued"
    end

    -- Over the cap the job stays queued -- never an error -- and a later poll
    -- starts it. Soft: two requests can both read this before either spawns.
    -- Starts go to whoever polls next, so the queue is not strictly
    -- first-come, which is why the page is never told a position.
    if M.active_count(db) >= M.MAX_CONCURRENT then return job end

    thread.spawn(name, "jobs/archive.lua", { job = job.id })
    return job
end

-- Returns the job row to watch. Never builds anything itself.
function M.enqueue(db, user, folder)
    local signature = M.signature(db, user, folder)
    local existing = reusable(db, user, folder, signature)
    if existing then return existing end

    local r = db:execute([[
        INSERT INTO archive_jobs (owner_id, folder_id, status, signature, created_at)
        VALUES (?, ?, 'queued', ?, ?)
    ]], { user.id, folder_key(folder), signature, os.time() })

    local job = db:query("SELECT * FROM archive_jobs WHERE id = ?", { r.last_insert_rowid })[1]
    return M.ensure_running(db, job)
end

-- Ownership is part of the lookup, so a guessed id reveals nothing.
function M.get_job(db, user, id)
    id = tonumber(id)
    if not id then return nil end
    return db:query("SELECT * FROM archive_jobs WHERE id = ? AND owner_id = ?",
        { id, user.id })[1]
end

-- Claims one specific job. Conditional on it still being queued, so two
-- tasks racing on the same row cannot both build it.
function M.claim(db, id)
    local r = db:execute(
        "UPDATE archive_jobs SET status = 'building' WHERE id = ? AND status = 'queued'", { id })
    if r.changes == 0 then return nil end
    return db:query("SELECT * FROM archive_jobs WHERE id = ?", { id })[1]
end

function M.mark_ready(db, id, built)
    db:execute([[
        UPDATE archive_jobs SET status = 'ready', path = ?, filename = ?,
               content_type = ?, finished_at = ? WHERE id = ?
    ]], { built.path, built.filename, built.content_type, os.time(), id })
end

function M.mark_failed(db, id, err)
    db:execute(
        "UPDATE archive_jobs SET status = 'failed', error = ?, finished_at = ? WHERE id = ?",
        { tostring(err), os.time(), id })
end

return M
