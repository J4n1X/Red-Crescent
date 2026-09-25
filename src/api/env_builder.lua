local shared = ...

local search, include = shared.search, shared.include
local set_cookie, send_file = shared.set_cookie, shared.send_file
local native_require = shared.native_require
local mark, unmodified = shared.mark_finalizers, shared.unmodified

local TABLES = { --[[TABLES]] }
local FUNCS = { --[[FUNCS]] }

local next, select, error, tostring, rawload, type = next, select, error, tostring, load, type
local rawget, setmetatable = rawget, setmetatable
local G, VERSION = _G, _VERSION
local pkg_path, pkg_cpath = package.path, package.cpath
local pkg_config, loadlib = package.config, package.loadlib

-- A finalizer left for the incremental collector would run inside a later
-- request, so anything that can create one marks this request for a full
-- collection before the state is handed on.
local function marked(f) return function(...) mark() return f(...) end end

local funcs = {}
for i = 1, #FUNCS do funcs[i] = G[FUNCS[i]] end
for i = 1, #FUNCS do
    if FUNCS[i] == "setmetatable" then
        funcs[i] = function(t, mt)
            if type(mt) == "table" and rawget(mt, "__gc") ~= nil then mark() end
            return setmetatable(t, mt)
        end
    end
end

-- Resolved once per state. A name absent on this backend (utf8 on LuaJIT)
-- stays a hole and drops out of the constructors below.
local sources, ismodule = {}, {}
for i = 1, #TABLES do
    local name = TABLES[i]
    local source = G[name]
    if type(source) == "table" then
        if name == "io" then
            local wrapped = {}
            for k, v in next, source do wrapped[k] = v end
            for _, k in next, { "open", "lines", "popen", "tmpfile", "input", "output" } do
                if type(source[k]) == "function" then wrapped[k] = marked(source[k]) end
            end
            source = wrapped
        end
        sources[i] = source
        -- Only what stock require already resolves stays require-able.
        ismodule[i] = package.loaded[name] ~= nil
    end
end

local copies, modules, sizes = {}, {}, {}
local function copy_of(i)
    local copy, n = {}, 0
    for k, v in next, sources[i] do
        copy[k] = v
        n = n + 1
    end
    copies[i], sizes[i] = copy, n
    if ismodule[i] then modules[i] = copy end
end
for i = 1, #TABLES do
    if sources[i] ~= nil then copy_of(i) end
end

local function reset()
    for i = 1, #TABLES do
        local copy = copies[i]
        if copy ~= nil and not unmodified(copy, sources[i], sizes[i]) then copy_of(i) end
    end
end

local function build(request)
    local env
    local preload, loading = {}, {}
    local loaded = { --[[LOADED]] _G = nil }
    env = {
        --[[ENV]]
        _G = nil,
        _VERSION = VERSION,
        request = request,
        -- `loaded` is this request's, so module state dies with it. `searchers`
        -- is absent rather than copied: require below does not consult it.
        package = {
            path = pkg_path,
            cpath = pkg_cpath,
            config = pkg_config,
            loaded = loaded,
            preload = preload,
            loadlib = loadlib,
        },
        require = nil,
        include = nil,
        load = nil,
        loadstring = nil,
        response = {
            status = 200,
            headers = {},
            set_cookie = set_cookie,
            send_file = send_file,
        },
    }
    env._G = env
    loaded._G = env

    --[[LOAD_SHIM]]

    local function record(name, result)
        if result == nil then result = true end
        loaded[name], loading[name] = result, nil
        return result
    end

    env.require = function(name)
        local module = loaded[name]
        if module ~= nil then return module end
        if loading[name] then
            error("loop or previous error loading module '" .. tostring(name) .. "'", 2)
        end
        loading[name] = true
        local loader = preload[name]
        if loader ~= nil then return record(name, loader(name, ":preload:")) end
        local path
        loader, path = search(name, env)
        if loader ~= nil then return record(name, loader(name, path)) end
        loading[name] = nil
        -- Native modules only, and only with --c-module-dir, which pools nothing.
        if native_require ~= nil then return native_require(name) end
        error("module '" .. tostring(name) .. "' not found:" .. tostring(path), 2)
    end

    env.include = function(rel) return include(rel, env) end

    return env
end

return build, reset
