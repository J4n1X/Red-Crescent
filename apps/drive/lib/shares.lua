-- Public share links for a single file or a whole folder: unguessable
-- tokens, optional expiry, download counting.
local M = {}

-- kind is "file" or "folder"; target is the row being shared.
-- expires_secs nil = never expires. Returns the token.
function M.create(db, user, kind, target, expires_secs)
    local token = crypto.random_token(16)
    local expires_at = expires_secs and (os.time() + expires_secs) or sqlite.NULL
    db:execute([[
        INSERT INTO shares (token, file_id, folder_id, created_by, expires_at, created_at)
        VALUES (?, ?, ?, ?, ?, ?)
    ]], {
        token,
        kind == "file" and target.id or sqlite.NULL,
        kind == "folder" and target.id or sqlite.NULL,
        user.id,
        expires_at,
        os.time(),
    })
    return token
end

function M.for_target(db, user, kind, target)
    local column = kind == "folder" and "folder_id" or "file_id"
    return db:query(
        "SELECT * FROM shares WHERE " .. column .. " = ? AND created_by = ? ORDER BY created_at DESC",
        { target.id, user.id })
end

-- Every link this user has handed out, newest first, with the name of what it
-- points at. Expired rows are included so they can be cleaned up by hand;
-- they already fail at access time.
function M.all_for_user(db, user)
    return db:query([[
        SELECT s.token, s.expires_at, s.downloads, s.created_at,
               s.file_id, s.folder_id,
               COALESCE(f.name, d.name) AS target_name,
               f.size AS file_size
          FROM shares s
          LEFT JOIN files   f ON f.id = s.file_id
          LEFT JOIN folders d ON d.id = s.folder_id
         WHERE s.created_by = ?
         ORDER BY s.created_at DESC
    ]], { user.id })
end

-- Resolves a token, enforcing expiry at access time. Returns a table with
-- `kind`, `owner_id`, the share metadata, and either `file` or `folder`.
function M.lookup(db, token)
    if type(token) ~= "string" or #token == 0 or #token > 128 then return nil end
    local share = db:query("SELECT * FROM shares WHERE token = ?", { token })[1]
    if not share then return nil end
    if share.expires_at and share.expires_at <= os.time() then return nil end

    local result = {
        token = share.token,
        owner_id = share.created_by,
        expires_at = share.expires_at,
        downloads = share.downloads,
        created_at = share.created_at,
    }
    if share.folder_id then
        result.kind = "folder"
        result.folder = db:query("SELECT * FROM folders WHERE id = ?", { share.folder_id })[1]
        if not result.folder then return nil end
    else
        result.kind = "file"
        result.file = db:query("SELECT * FROM files WHERE id = ?", { share.file_id })[1]
        if not result.file then return nil end
    end
    return result
end

function M.revoke(db, user, token)
    db:execute("DELETE FROM shares WHERE token = ? AND created_by = ?", { token, user.id })
end

function M.count_download(db, token)
    db:execute("UPDATE shares SET downloads = downloads + 1 WHERE token = ?", { token })
end

return M
