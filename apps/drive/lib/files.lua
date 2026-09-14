-- File and folder model. Every operation takes the owning user and checks
-- ownership in SQL — a valid session can never touch another user's rows.
local dbm = require("lib.db")
local config = require("config")
local M = {}

function M.disk_path(file)
    return dbm.FILES_DIR .. "/" .. file.stored_name
end

-- nil/"" folder id means the root; otherwise the folder must belong to user.
function M.get_folder(db, user, folder_id)
    local id = tonumber(folder_id)
    if not id then return nil end
    return db:query("SELECT * FROM folders WHERE id = ? AND owner_id = ?", { id, user.id })[1]
end

function M.get_file(db, user, file_id)
    local id = tonumber(file_id)
    if not id then return nil end
    return db:query("SELECT * FROM files WHERE id = ? AND owner_id = ?", { id, user.id })[1]
end

-- True when `folder_id` is `root_id` itself or nested inside it. Used to keep
-- public folder shares from being navigated outside their subtree.
function M.is_within(db, owner_id, folder_id, root_id)
    local current = folder_id
    local guard = 0
    while current and guard < 64 do
        if current == root_id then return true end
        local row = db:query(
            "SELECT parent_id FROM folders WHERE id = ? AND owner_id = ?", { current, owner_id })[1]
        if not row then return false end
        current = row.parent_id
        guard = guard + 1
    end
    return false
end

function M.breadcrumbs(db, user, folder)
    local crumbs = {}
    local current = folder
    local guard = 0
    while current and guard < 64 do
        table.insert(crumbs, 1, current)
        current = current.parent_id
            and db:query("SELECT * FROM folders WHERE id = ? AND owner_id = ?",
                { current.parent_id, user.id })[1]
            or nil
        guard = guard + 1
    end
    return crumbs
end

-- Lists (folders, files) inside `folder` (nil = root).
function M.list(db, user, folder)
    if folder then
        return db:query("SELECT * FROM folders WHERE owner_id = ? AND parent_id = ? ORDER BY name",
                { user.id, folder.id }),
            db:query("SELECT * FROM files WHERE owner_id = ? AND folder_id = ? ORDER BY name",
                { user.id, folder.id })
    end
    return db:query("SELECT * FROM folders WHERE owner_id = ? AND parent_id IS NULL ORDER BY name",
            { user.id }),
        db:query("SELECT * FROM files WHERE owner_id = ? AND folder_id IS NULL ORDER BY name",
            { user.id })
end

function M.all_folders(db, user)
    return db:query("SELECT * FROM folders WHERE owner_id = ? ORDER BY name", { user.id })
end

-- Every folder with its full path, parents before children. A flat list sorted
-- by name cannot tell two folders called "photos" apart, nor say where either
-- one lives. Paths are unique, since a parent cannot hold two folders of the
-- same name, which is what lets the browser match a subtree by prefix.
function M.folder_paths(db, user)
    local rows = db:query(
        "SELECT id, parent_id, name FROM folders WHERE owner_id = ? ORDER BY name", { user.id })

    local children = {}
    for _, row in ipairs(rows) do
        local key = row.parent_id or 0
        children[key] = children[key] or {}
        table.insert(children[key], row)
    end

    local out = {}
    local function walk(parent_key, prefix, depth)
        -- Depth-capped like is_within: corrupt data must not spin forever.
        if depth > 64 then return end
        for _, row in ipairs(children[parent_key] or {}) do
            row.path = prefix .. "/" .. row.name
            row.depth = depth
            table.insert(out, row)
            walk(row.id, row.path, depth + 1)
        end
    end
    walk(0, "", 0)
    return out
end

-- Bytes currently stored by a user.
function M.usage(db, user)
    return db:query("SELECT COALESCE(SUM(size), 0) AS n FROM files WHERE owner_id = ?",
        { user.id })[1].n
end

-- The user's quota in bytes, or nil for unlimited. A per-user value set by an
-- admin wins over the app-wide default.
function M.quota(db, user)
    local row = db:query("SELECT quota_bytes FROM users WHERE id = ?", { user.id })[1]
    local quota = row and row.quota_bytes or config.default_quota_bytes
    if not quota or quota <= 0 then return nil end
    return quota
end

local function valid_name(name)
    return type(name) == "string" and #name > 0 and #name <= 128
end

-- Run fn; only a UNIQUE-constraint failure is reported as a conflict
-- (returns false), anything else is re-raised so real bugs stay visible.
local function try_unique(fn)
    local ok, err = pcall(fn)
    if ok then return true end
    if tostring(err):find("UNIQUE") then return false end
    error(err)
end

local function select_folder(db, user, parent_id, name)
    if parent_id then
        return db:query(
            "SELECT * FROM folders WHERE owner_id = ? AND parent_id = ? AND name = ?",
            { user.id, parent_id, name })[1]
    end
    return db:query(
        "SELECT * FROM folders WHERE owner_id = ? AND parent_id IS NULL AND name = ?",
        { user.id, name })[1]
end

-- Get-or-create a folder under `parent` (nil = root). Safe against the
-- concurrent-create race: on a UNIQUE conflict it re-selects the winner.
-- `cache` is optional; a bulk upload passes one so that a deep tree costs a
-- single lookup per folder instead of one per file inside it.
function M.ensure_folder(db, user, parent, name, cache)
    local parent_id = parent and parent.id or nil
    local key = cache and ((parent_id or "root") .. "/" .. name)
    if cache and cache[key] then return cache[key] end

    local folder = select_folder(db, user, parent_id, name)
    if not folder then
        local id, err = M.create_folder(db, user, parent, name)
        if id then
            folder = { id = id, owner_id = user.id, parent_id = parent_id, name = name }
        else
            folder = select_folder(db, user, parent_id, name)
            if not folder then return nil, err end
        end
    end
    if cache then cache[key] = folder end
    return folder
end

-- Splits an upload's filename into sanitized path segments. Folder uploads
-- (webkitdirectory) send relative paths like "proj/src/main.lua". Folder
-- names only ever exist as DB rows — they never touch disk paths — so even
-- a hostile segment is cosmetic, not a traversal risk.
local function split_upload_path(filename)
    local segments = {}
    for segment in (filename or ""):gmatch("[^/\\]+") do
        if segment ~= "." then
            if segment:match("^%.+$") then segment = "_" end
            if #segment > 128 then segment = segment:sub(-128) end
            table.insert(segments, segment)
        end
    end
    return segments
end

-- Stores one spooled upload. A relative path in the filename creates the
-- virtual folder chain beneath `folder` first. The DB row is inserted
-- *before* the rename so the cleanup job can never see an untracked stored
-- file. Returns the stored display name, or nil + error message.
-- `budget` is an optional { remaining = bytes } table threaded through a
-- batch: checking it here (rather than re-summing the files table for every
-- file) keeps a 4000-file upload from doing 4000 aggregate queries.
function M.store_upload(db, user, folder, upload, cache, budget)
    if budget and budget.remaining and upload.size > budget.remaining then
        return nil, "storage quota exceeded"
    end
    local segments = split_upload_path(upload.filename)
    local name = table.remove(segments) or "unnamed"
    if #segments > 32 then
        return nil, "folder nesting too deep: " .. upload.filename
    end
    for _, segment in ipairs(segments) do
        local sub, err = M.ensure_folder(db, user, folder, segment, cache)
        if not sub then
            return nil, err or ("could not create folder " .. segment)
        end
        folder = sub
    end

    local stored = crypto.random_token(16)
    local folder_id = folder and folder.id or sqlite.NULL

    local final_name
    for attempt = 1, 100 do
        local try = attempt == 1 and name or (name .. " (" .. attempt .. ")")
        if try_unique(function()
            db:execute([[
                INSERT INTO files (owner_id, folder_id, name, stored_name, size, content_type, created_at)
                VALUES (?, ?, ?, ?, ?, ?, ?)
            ]], { user.id, folder_id, try, stored, upload.size,
                  upload.content_type or sqlite.NULL, os.time() })
        end) then
            final_name = try
            break
        end
    end
    if not final_name then
        return nil, "could not find a free name for " .. name
    end

    local moved, err = os.rename(upload.path, M.disk_path({ stored_name = stored }))
    if not moved then
        db:execute("DELETE FROM files WHERE stored_name = ?", { stored })
        return nil, "failed to store " .. name .. ": " .. tostring(err)
    end
    if budget and budget.remaining then
        budget.remaining = budget.remaining - upload.size
    end
    return final_name
end

function M.delete_file(db, user, file)
    db:execute("DELETE FROM files WHERE id = ? AND owner_id = ?", { file.id, user.id })
    os.remove(M.disk_path(file))
end

-- Returns true, or nil + error message.
function M.rename_file(db, user, file, new_name)
    if not valid_name(new_name) then return nil, "Invalid name." end
    if not try_unique(function()
        db:execute("UPDATE files SET name = ? WHERE id = ? AND owner_id = ?",
            { new_name, file.id, user.id })
    end) then
        return nil, "A file with that name already exists here."
    end
    return true
end

-- dest may be nil (root). Returns true, or nil + error message.
function M.move_file(db, user, file, dest)
    local dest_id = dest and dest.id or sqlite.NULL
    if not try_unique(function()
        db:execute("UPDATE files SET folder_id = ? WHERE id = ? AND owner_id = ?",
            { dest_id, file.id, user.id })
    end) then
        return nil, "A file with that name already exists in the target folder."
    end
    return true
end

-- Returns the new folder id, or nil + error message.
function M.create_folder(db, user, parent, name)
    if not valid_name(name) then return nil, "Invalid folder name." end
    local parent_id = parent and parent.id or sqlite.NULL
    local result
    if not try_unique(function()
        result = db:execute([[
            INSERT INTO folders (owner_id, parent_id, name, created_at) VALUES (?, ?, ?, ?)
        ]], { user.id, parent_id, name, os.time() })
    end) then
        return nil, "A folder with that name already exists here."
    end
    return result.last_insert_rowid
end

-- Returns true, or nil + error message.
function M.rename_folder(db, user, folder, new_name)
    if not valid_name(new_name) then return nil, "Invalid folder name." end
    if not try_unique(function()
        db:execute("UPDATE folders SET name = ? WHERE id = ? AND owner_id = ?",
            { new_name, folder.id, user.id })
    end) then
        return nil, "A folder with that name already exists here."
    end
    return true
end

-- dest may be nil (root). Returns true, or nil + error message.
function M.move_folder(db, user, folder, dest)
    -- Into itself or its own subtree would cut the whole branch loose in a
    -- cycle that no listing can reach and no breadcrumb can escape.
    if dest and (dest.id == folder.id or M.is_within(db, user.id, dest.id, folder.id)) then
        return nil, "A folder cannot be moved into itself."
    end
    local dest_id = dest and dest.id or sqlite.NULL
    if not try_unique(function()
        db:execute("UPDATE folders SET parent_id = ? WHERE id = ? AND owner_id = ?",
            { dest_id, folder.id, user.id })
    end) then
        return nil, "A folder with that name already exists in the target folder."
    end
    return true
end

-- Recursively deletes a folder: all contained files (rows + disk) and
-- subfolders. Iterative so deep trees can't blow the stack.
function M.delete_folder(db, user, folder)
    local ids = { folder.id }
    local stack = { folder.id }
    while #stack > 0 do
        local id = table.remove(stack)
        for _, child in ipairs(db:query(
            "SELECT id FROM folders WHERE parent_id = ? AND owner_id = ?", { id, user.id })) do
            table.insert(ids, child.id)
            table.insert(stack, child.id)
        end
    end
    for _, id in ipairs(ids) do
        for _, file in ipairs(db:query(
            "SELECT * FROM files WHERE folder_id = ? AND owner_id = ?", { id, user.id })) do
            M.delete_file(db, user, file)
        end
    end
    -- Children were appended after their parents; delete in reverse.
    for i = #ids, 1, -1 do
        db:execute("DELETE FROM folders WHERE id = ? AND owner_id = ?", { ids[i], user.id })
    end
end

return M
