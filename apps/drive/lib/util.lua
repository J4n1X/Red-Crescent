-- Small shared helpers for Drive.
local config = require("config")
local M = {}

M.config = config

-- True when the visitor reached us over HTTPS. The app always speaks plain
-- HTTP, so the proxy's header is the only evidence. Assuming true instead
-- would mark session cookies Secure in local development, and the browser
-- would then refuse to send them back over http://.
function M.is_https()
    local forwarded = request.headers["X-Forwarded-Proto"]
    return forwarded ~= nil and (forwarded:match("^[^,%s]+") or ""):lower() == "https"
end

-- The visitor's address, for throttling.
--
-- Headers first: behind a proxy the socket peer is the proxy, so every visitor
-- would share one bucket. They are trustworthy only because nginx sets them
-- and the app is not reachable directly. remote_addr is the fallback for a
-- deployment with no proxy, where the headers are absent -- and where they are
-- also forgeable, which only an operator-set trust setting could fix.
function M.client_ip()
    local real = request.headers["X-Real-IP"]
    if real and real ~= "" then return real end
    local forwarded = request.headers["X-Forwarded-For"]
    if forwarded and forwarded ~= "" then
        -- Left-most entry is the original client.
        return forwarded:match("^%s*([^,%s]+)") or "unknown"
    end
    return request.remote_addr or "unknown"
end

-- Base URL for links that leave the app (share links, mails, ...).
--
-- Prefers the configured public address; otherwise reconstructs it from the
-- request. Behind a reverse proxy the X-Forwarded-* headers carry the address
-- the visitor actually used, which is why they are consulted before Host —
-- without them every share link would carry the internal bind address.
function M.base_url()
    if config.share_base_url and config.share_base_url ~= "" then
        return (config.share_base_url:gsub("/+$", ""))
    end

    local forwarded_proto = request.headers["X-Forwarded-Proto"]
    local scheme = forwarded_proto and forwarded_proto:match("^[^,%s]+") or "http"
    local host = request.headers["X-Forwarded-Host"] or request.headers.host or "localhost"
    host = host:match("^[^,%s]+") or "localhost"
    return scheme .. "://" .. host
end

function M.urlencode(s)
    return (tostring(s):gsub("[^%w%-_%.~]", function(c)
        return string.format("%%%02X", c:byte())
    end))
end

function M.human_size(bytes)
    bytes = tonumber(bytes) or 0
    if bytes < 1024 then return bytes .. " B" end
    local units = { "KiB", "MiB", "GiB", "TiB" }
    local value = bytes
    local unit = "B"
    for _, u in ipairs(units) do
        if value < 1024 then break end
        value = value / 1024
        unit = u
    end
    return string.format("%.1f %s", value, unit)
end

function M.format_time(ts)
    return os.date("%Y-%m-%d %H:%M", ts)
end

-- Renders the ?msg= / ?err= flash boxes (escaped).
function M.flash_html()
    local out = {}
    if request.query.msg and request.query.msg ~= "" then
        table.insert(out, '<div class="flash flash-ok">' .. html_escape(request.query.msg) .. "</div>")
    end
    if request.query.err and request.query.err ~= "" then
        table.insert(out, '<div class="flash flash-err">' .. html_escape(request.query.err) .. "</div>")
    end
    return table.concat(out)
end

return M
