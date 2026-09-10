-- Builds one archive, then exits.
--
-- Spawned per job by lib/archive.lua with the job id in `args`; nothing polls
-- and nothing idles. The thread is named after its job, so a double-click
-- cannot start a second build of the same export.

local dbm = require("lib.db")
local archive = require("lib.archive")

local job_id = args and tonumber(args.job)
if not job_id then
    error("archive: spawned without a job id in args")
end

local db = dbm.open()

-- Conditional on the row still being queued, so a racing task backs off.
local job = archive.claim(db, job_id)
if not job then
    return { job = job_id, claimed = false }
end

local user = db:query("SELECT * FROM users WHERE id = ?", { job.owner_id })[1]
if not user then
    archive.mark_failed(db, job.id, "the account no longer exists")
    return { job = job_id, status = "failed" }
end

local folder = nil
if job.folder_id then
    folder = db:query("SELECT * FROM folders WHERE id = ? AND owner_id = ?",
        { job.folder_id, job.owner_id })[1]
    if not folder then
        archive.mark_failed(db, job.id, "the folder no longer exists")
        return { job = job_id, status = "failed" }
    end
end

-- pcall so a broken export is recorded, not left marked 'building'.
local ok, built, err = pcall(archive.build, db, user, folder)
if not ok then
    archive.mark_failed(db, job.id, built)
    log.error("drive: archive job " .. job.id .. " crashed: " .. tostring(built))
    return { job = job_id, status = "failed" }
end
if not built then
    archive.mark_failed(db, job.id, err or "the archive could not be built")
    return { job = job_id, status = "failed" }
end

archive.mark_ready(db, job.id, built)
print("archive: job " .. job.id .. " ready (" .. built.filename .. ")")
return { job = job_id, status = "ready", filename = built.filename }
