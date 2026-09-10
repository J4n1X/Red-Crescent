-- Authentication: sessions, login/registration, CSRF protection.
local config = require("config")
local util = require("lib.util")
local M = {}

M.SESSION_TTL = config.session_ttl or 7 * 24 * 3600

-- Returns the logged-in, approved user row (with csrf_token) or nil.
function M.current_user(db)
    local token = request.cookies.session
    if type(token) ~= "string" or #token == 0 then return nil end
    local rows = db:query([[
        SELECT u.id, u.username, u.is_admin, u.approved, s.csrf_token
        FROM sessions s JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = ? AND s.expires_at > ?
    ]], { crypto.sha256(token), os.time() })
    local user = rows[1]
    if user and user.approved == 1 then return user end
    return nil
end

function M.require_login(db)
    local user = M.current_user(db)
    if not user then redirect("/login.lhtml") end
    return user
end

function M.start_session(db, user_id)
    local token = crypto.random_token(32)
    db:execute([[
        INSERT INTO sessions (token_hash, user_id, csrf_token, expires_at, created_at)
        VALUES (?, ?, ?, ?, ?)
    ]], { crypto.sha256(token), user_id, crypto.random_token(16), os.time() + M.SESSION_TTL, os.time() })
    response.set_cookie{
        name = "session", value = token, path = "/",
        http_only = true, same_site = "lax", max_age = M.SESSION_TTL,
        secure = util.is_https(),
    }
end

-- Failed-login throttling.
--
-- nginx also rate limits these endpoints, but that protection disappears the
-- moment Drive is run without that proxy in front, so the app enforces its
-- own. Counted per IP *and* per username: the first stops one host guessing
-- many passwords, the second stops a distributed attempt on one account.
M.LOGIN_WINDOW = config.login_window or 900
M.LOGIN_MAX_PER_IP = config.login_max_per_ip or 10
M.LOGIN_MAX_PER_USER = config.login_max_per_user or 20

local function recent_failures(db, since)
    local ip = util.client_ip()
    local by_ip = db:query(
        "SELECT COUNT(*) AS n FROM login_attempts WHERE ip = ? AND at > ?", { ip, since })[1].n
    return ip, by_ip
end

function M.login_throttled(db, username)
    local since = os.time() - M.LOGIN_WINDOW
    local _, by_ip = recent_failures(db, since)
    if by_ip >= M.LOGIN_MAX_PER_IP then return true end
    if username and username ~= "" then
        local by_user = db:query(
            "SELECT COUNT(*) AS n FROM login_attempts WHERE username = ? AND at > ?",
            { username, since })[1].n
        if by_user >= M.LOGIN_MAX_PER_USER then return true end
    end
    return false
end

local function record_failure(db, username)
    db:execute("INSERT INTO login_attempts (ip, username, at) VALUES (?, ?, ?)",
        { util.client_ip(), username or "", os.time() })
end

-- Returns the user row on success, or nil + error message.
function M.login(db, username, password)
    username = username or ""

    -- Checked before the password is verified, so a throttled attacker also
    -- stops consuming argon2 CPU time -- otherwise the login endpoint is a
    -- cheap way to make the server do expensive work.
    if M.login_throttled(db, username) then
        return nil, "Too many failed attempts. Please wait a few minutes and try again."
    end

    local user = db:query("SELECT * FROM users WHERE username = ?", { username })[1]
    if not user or not crypto.password_verify(password or "", user.password_hash) then
        record_failure(db, username)
        return nil, "Invalid username or password."
    end
    if user.approved ~= 1 then
        return nil, "Your account is awaiting admin approval."
    end

    -- A success clears the slate for this address, so one fat-fingered
    -- session does not lock the legitimate user out afterwards.
    db:execute("DELETE FROM login_attempts WHERE ip = ? OR username = ?",
        { util.client_ip(), username })
    M.start_session(db, user.id)
    return user
end

-- Returns is_admin (true for the very first account), or nil + error message.
function M.register(db, username, password)
    username = username or ""
    password = password or ""
    if #username < 3 or #username > 32 or not username:match("^[%w_%-]+$") then
        return nil, "Username must be 3-32 characters: letters, digits, _ or -."
    end
    if #password < 8 then
        return nil, "Password must be at least 8 characters."
    end
    local first = db:query("SELECT COUNT(*) AS n FROM users")[1].n == 0
    local flag = first and 1 or 0
    local ok, err = pcall(function()
        db:execute([[
            INSERT INTO users (username, password_hash, is_admin, approved, created_at)
            VALUES (?, ?, ?, ?, ?)
        ]], { username, crypto.password_hash(password), flag, flag, os.time() })
    end)
    if not ok then
        if tostring(err):find("UNIQUE") then
            return nil, "That username is already taken."
        end
        error(err)
    end
    return first
end

function M.logout(db)
    local token = request.cookies.session
    if type(token) == "string" and #token > 0 then
        db:execute("DELETE FROM sessions WHERE token_hash = ?", { crypto.sha256(token) })
    end
    response.set_cookie{
        name = "session", value = "", path = "/",
        http_only = true, same_site = "lax", max_age = 0,
        secure = util.is_https(),
    }
end

-- Aborts the request unless the POSTed csrf field matches the session token.
function M.check_csrf(user)
    local sent = request.body.csrf
    if type(sent) ~= "string" or not crypto.constant_time_equals(sent, user.csrf_token) then
        response.status = 403
        print("403 Forbidden: CSRF token mismatch")
        exit()
    end
end

function M.csrf_field(user)
    return '<input type="hidden" name="csrf" value="' .. user.csrf_token .. '">'
end

return M
