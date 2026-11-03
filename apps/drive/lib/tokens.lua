-- API tokens for non-browser clients. Only the sha256 is stored; the token
-- itself is shown once at creation.
local M = {}

M.PREFIX = "drv_"

-- Returns the plaintext token.
function M.create(db, user, name)
    local token = M.PREFIX .. crypto.random_token(32)
    db:execute([[
        INSERT INTO api_tokens (user_id, name, token_hash, created_at) VALUES (?, ?, ?, ?)
    ]], { user.id, name, crypto.sha256(token), os.time() })
    return token
end

function M.list(db, user)
    return db:query(
        "SELECT id, name, created_at, last_used_at FROM api_tokens WHERE user_id = ? ORDER BY created_at DESC",
        { user.id })
end

function M.revoke(db, user, id)
    db:execute("DELETE FROM api_tokens WHERE id = ? AND user_id = ?", { tonumber(id) or -1, user.id })
end

-- The approved user behind `Authorization: Bearer <token>`, or nil. Cookies
-- are never consulted, which is why API requests need no CSRF check.
function M.current_user(db)
    local header = request.headers["Authorization"] or ""
    local token = header:match("^[Bb]earer%s+(%S+)%s*$")
    if not token or #token > 128 then return nil end
    local user = db:query([[
        SELECT u.id, u.username, u.is_admin, u.approved, t.id AS token_id
        FROM api_tokens t JOIN users u ON u.id = t.user_id
        WHERE t.token_hash = ?
    ]], { crypto.sha256(token) })[1]
    if not user or user.approved ~= 1 then return nil end
    db:execute("UPDATE api_tokens SET last_used_at = ? WHERE id = ?", { os.time(), user.token_id })
    return user
end

return M
