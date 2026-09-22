# Red Crescent

A Rust-powered web server that renders dynamic HTML using embedded Lua scripts. Think PHP, but with Lua and Rust!
Its intended purpose is to be used behind another webserver such as Nginx or Apache2 (though it can also serve static files itself).

## Features

- **🌙 Lua templating**: Embed Lua in HTML with `<?lua ... ?>` blocks and `<?lua= expr ?>` inline expressions, **HTML-escaped by default**
- **🔀 Real control flow**: Loops and conditionals span blocks, PHP-style: `<?lua for ... do ?> <li>...</li> <?lua end ?>`
- **📋 Request API**: Method, path, headers (case-insensitive), query params (incl. repeated), cookies, form/JSON bodies, **multipart file uploads**
- **⚙️ Response control**: Status codes, headers, cookies, `redirect()`/`exit()`, and `response.send_file()` for efficient downloads
- **🗄️ SQLite built in**: `sqlite.open()` with injection-safe prepared statements, confined to the data directory
- **🔐 Crypto built in**: argon2id password hashing, secure random tokens, sha256, constant-time comparison
- **🧵 Background threads**: `thread.spawn("worker", "jobs/worker.lua", args)` — a long-lived Lua instance on its own OS thread, with arguments in, a return value out, and `thread.join()` to wait on it
- **🧩 Includes & modules**: `include("partials/nav.lhtml")` for partials, `require("utils")` for `.lua` modules
- **🛡️ Guard rails**: Per-request execution timeout and memory limit, so a buggy template can't take down a worker
- **🚀 Non-blocking**: Lua runs on blocking threads, each request isolated in its own environment; slow templates don't stall other requests
- **📁 Static files**: CSS/JS/images served alongside templates (`.lua` sources and dotfiles are never served)
- **📝 Server logging**: `log.info()`, `log.debug()`, etc. from Lua

Want proof it's enough to build something real? **[apps/drive](apps/drive/)** is a complete
multi-user file-sharing app — auth, sessions, folders, uploads, share links — written entirely
in Lua on these APIs: `cargo run --release -- --serve-dir apps/drive`

## Quick Start

```bash
git clone <repository-url>
cd lua-html-renderer
cargo run --release -- --dev
```

Then open:

- Home (includes + require demo): `http://127.0.0.1:8080/`
- Kitchen-sink demo: `http://127.0.0.1:8080/demo.lhtml`
- Request API demo: `http://127.0.0.1:8080/demo2.lhtml`
- JSON API demo: `http://127.0.0.1:8080/api.lhtml`
- Lua environment info: `http://127.0.0.1:8080/info.lhtml`

`--dev` enables detailed error pages (with real `.lhtml` file/line numbers) and disables template caching so edits show up immediately. Without it you get generic error pages (details go to the log) and compiled templates are cached.

Building requires Lua 5.4 development headers (`liblua5.4-dev` on Debian/Ubuntu).

## Configuration

Settings come from three places, in decreasing priority: **command-line flags**,
**environment variables**, and an optional **`rc_config.lua` in the serve directory**.
Anything unset falls back to the default.

The config file is Lua, so limits read as arithmetic rather than digit counting, and because
it lives in the serve directory an application can ship the settings it needs — `apps/drive`
does exactly that, which is why it runs correctly with no flags at all:

```lua
-- apps/drive/rc_config.lua
return {
    bind = "127.0.0.1:8080",
    data_dir = "./data",
    max_upload_size = 8 * 1024 * 1024 * 1024,   -- Drive stores real folders
    max_upload_files = 20000,
    timeout_ms = 30000,
    threads = { "jobs/cleanup.lua" },
}
```

```bash
cargo run --release -- --serve-dir apps/drive     # limits come from the file
```

Every key is optional and uses the same name as the flag. `serve_dir` is the one setting the
file cannot provide (it is what tells the server where to *find* the file) — use
`--serve-dir` or `RC_SERVE_DIR`. Unknown keys and wrong types are startup errors rather than
silent no-ops, so a typo like `max_upload_sizes` stops the server with a message naming the
offending key instead of quietly leaving the real limit in place. Point elsewhere with
`--config <path>`, or ignore the file entirely with `--no-config`. `.lua` files are never
served over HTTP, so the config stays private even though it sits in the served directory.

All options as flags and environment variables (`--help` for the full list):

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--bind` | `RC_BIND` | `127.0.0.1:8080` | Address to listen on |
| `--serve-dir` | `RC_SERVE_DIR` | `./demos` | Directory served |
| `--workers` | `RC_WORKERS` | `4` | HTTP worker threads |
| `--timeout-ms` | `RC_TIMEOUT_MS` | `5000` | Lua execution time limit per request |
| `--memory-limit-mb` | `RC_MEMORY_LIMIT_MB` | `64` | Lua memory limit per request |
| `--max-body-size` | `RC_MAX_BODY_SIZE` | `1048576` | Non-multipart body cap in bytes (413 beyond) |
| `--data-dir` | `RC_DATA_DIR` | `./data` | Writable sandbox for sqlite/uploads/send_file (must not be inside the serve dir) |
| `--max-upload-size` | `RC_MAX_UPLOAD_SIZE` | `268435456` | Total multipart upload cap in bytes |
| `--max-upload-files` | `RC_MAX_UPLOAD_FILES` | `256` | Max file parts per multipart request |
| `--thread` | `RC_THREADS` | — | Background Lua thread script spawned at boot, repeatable |
| `--thread-timeout-ms` | `RC_THREAD_TIMEOUT_MS` | — | Deadline for one awake stretch of a thread; unset or `0` means none |
| `--thread-memory-limit-mb` | `RC_THREAD_MEMORY_LIMIT_MB` | `256` | Lua memory limit for a background thread instance |
| `--sqlite-idle-connections` | `RC_SQLITE_IDLE_CONNECTIONS` | `2` | Warm SQLite connections kept per database, per worker thread; `0` disables reuse |
| `--c-module-dir` | `RC_C_MODULE_DIRS` | — | Directory of native `.so` Lua modules `require` may load, repeatable (off by default; needs a `--features c-modules` build) |
| `--lua-pool` | `RC_LUA_POOL` | `true` | Reuse Lua states between requests; forced off when C modules are enabled |
| `--lua-pool-max-requests` | `RC_LUA_POOL_MAX_REQUESTS` | `10000` | Requests one pooled state serves before it is retired |
| `--static-files` | `RC_STATIC_FILES` | `true` | Serve non-`.lhtml` files |
| `--index` | `RC_INDEX` | `index.lhtml` | File served for `/` and directories |
| `--fallback` | `RC_FALLBACK` | — | Template rendered when a path resolves to no file (front controller) |
| `--dev` | `RC_DEV` | `false` | Detailed error pages, no template cache |

Log verbosity is controlled with `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug cargo run`.

### Routing

A request path is resolved against the filesystem: the URL path is joined to the serve
directory, a directory gets `--index` appended, and anything that does not land on a real file
is a 404. `/about.lhtml` is a file called `about.lhtml`, and that is the whole rule.

`fallback` adds one escape hatch — a **front controller**. Set it to a template and every path
that resolves to no file renders that template instead of returning 404, with the original path
in `request.path`:

```lua
-- rc_config.lua
return { fallback = "app.lhtml" }
```

```html
<?lua
local token = request.path:match("^/s/(%w+)$")
if token then
    -- serve the share page
    return
end
response.status = 404
?>
Not found.
```

Real files always win — the fallback only sees paths that resolve to nothing, so static assets
and ordinary templates keep working untouched. Path traversal is still refused with a 403
before the fallback is considered. The template must be `.lhtml` and inside the serve
directory, and it is checked at startup, so a typo stops the server instead of breaking 404s
later.

Two things to know before turning it on. The response is a **200 unless the template says
otherwise** — a front controller owns its own statuses, including the 404 above. And the
fallback catches *everything* unresolved, `/favicon.ico` and stray crawler paths included, so
each of those now costs a Lua render rather than a cheap 404.

Route *matching* is deliberately the application's job: the platform provides the entry point,
and a Lua table of patterns is a better router than anything a config file could express.

## Template Syntax

Create a `.lhtml` file in the serve directory:

```html
<!DOCTYPE html>
<html>
<head><title>Hello Lua!</title></head>
<body>
    <h1>Hello, <?lua= request.query.name or "World" ?>!</h1>

    <ul>
        <?lua for _, item in ipairs({"Apples", "Bananas", "Cherries"}) do ?>
            <li><?lua= item ?></li>
        <?lua end ?>
    </ul>

    <?lua if request.query.debug then ?>
        <pre><?lua= json.encode(request.query, true) ?></pre>
    <?lua end ?>
</body>
</html>
```

- `<?lua ... ?>` runs statements. Use `print(...)` to write output.
- `<?lua= expr ?>` writes the expression's value, **HTML-escaped** (nothing for `nil`).
- `<?lua== expr ?>` writes it **unescaped**, for markup you built deliberately. This is the one
  thing to look for in a security review, so keep it rare.
- `print(...)` inside a `<?lua ... ?>` block does **not** escape, since it is how you emit markup
  you assembled yourself. Escape the values going into it with `html_escape()`.
- The whole file compiles to **one Lua chunk**: locals and control structures span blocks, and a top-level `return` stops rendering (like PHP's `exit`).
- A `?>` inside a Lua string or long bracket does not close the block. A `?>` inside a *comment* doesn't either — put the closing marker on its own line if a block ends with a comment.
- Error messages point at the real `.lhtml` file and line number.

## API Reference

### Request

```lua
request.method                    -- "GET", "POST", "PUT", "DELETE", ...
request.path                      -- URL path (percent-decoded)
request.remote_addr               -- peer IP of the socket, or nil
request.query.name                -- query parameter (last value if repeated)
request.query_all.name            -- array of all values for a repeated parameter
request.headers["User-Agent"]     -- case-insensitive lookup
request.cookies.session           -- request cookies
request.body.field                -- parsed body: form-urlencoded, JSON, or multipart fields
request.raw_body                  -- raw body string (non-multipart content types)
request.files                     -- array of uploaded files (multipart/form-data)
```

JSON bodies (`Content-Type: application/json`) are parsed into nested Lua tables. All HTTP methods reach the template — branch on `request.method`.

`request.remote_addr` is strictly the **socket** peer, IP only, and `nil` when the connection has
no address. Behind a reverse proxy that is the *proxy*, not the visitor — unwrapping
`X-Forwarded-For` is left to the application, because only the application knows which proxies it
trusts, and a platform that guessed would be handing every app the same spoofing bug. The usual
shape is headers first, `remote_addr` as the fallback; `apps/drive/lib/util.lua` does exactly
that.

**File uploads** are streamed to a spool inside the data directory before your template runs.
Each entry in `request.files` is `{field=, filename=, content_type=, size=, path=}`. Keep a
file by renaming it somewhere under the data directory: `os.rename(f.path, server.data_dir .. "/store/x")`
(same filesystem, atomic). Whatever is left in the spool is deleted when the request ends.
Folder uploads (`<input type="file" webkitdirectory>`) arrive as one part per file with the
relative path in `filename` (e.g. `proj/src/main.lua`) — passed through verbatim; treat it
as display data, never as a disk path.

**When an upload is too big** (over `--max-upload-size` or `--max-upload-files`) the server
returns a 413 page that *states the limit* and logs a warning naming the flag — an oversized
upload should never look like a mysterious connection failure. The size is checked against
`Content-Length` before any of the body is read, so browsers get the error page instead of a
reset connection, and a client using `Expect: 100-continue` never sends the body at all.
When a limit is only discovered mid-stream, the rest of the request is drained for up to 5
seconds (128 MiB) so the client can still read the response; a body too large to drain in
that window is cut off, which is unavoidable. Read the limits from Lua via
`server.max_upload_size` / `server.max_upload_files` / `server.max_body_size` to show them
in your forms.

### Response

```lua
response.status = 404
response.headers["Cache-Control"] = "no-cache"
response.headers["Content-Type"] = "application/json"  -- default: text/html; charset=utf-8

response.set_cookie{
    name = "session", value = "abc123",       -- required
    path = "/", domain = "example.com",       -- optional
    max_age = 3600,                           -- seconds
    http_only = true, secure = true,
    same_site = "lax",                        -- "strict" | "lax" | "none"
}

redirect("/login.lhtml")        -- 302 by default; redirect(url, 301) etc.
exit()                          -- stop rendering, send what was produced

-- Serve a file instead of the rendered output (ranges/ETag handled for you).
-- The path must be inside the data or serve directory.
response.send_file(server.data_dir .. "/store/report.pdf", {
    download_name = "Report.pdf",         -- optional: Content-Disposition attachment
    content_type = "application/pdf",     -- optional: override the guessed type
})
```

### Includes and modules

```lua
include("partials/header.lhtml")  -- renders in place; shares globals and output
local utils = require("utils")    -- loads <serve-dir>/utils.lua
```

Both are confined to the serve directory. Includes are capped at depth 16; `.lua` files can be `require`d but are never served over HTTP. Native `.so` modules are refused unless C modules are explicitly enabled — see [Native C modules](#native-c-modules).

**Modules are re-executed for every request**, and `package.loaded` is per-request, so a module's
top-level state lasts exactly one request. Use a database or a background thread for anything that
has to outlive one. The file itself is read and parsed once per process, not once per request.

### Environment differences from stock Lua

Each request runs in a closed environment rather than against shared globals (see
[Security Model](#security-model)), which shows in four places:

- `string.upper = f` changes `string.upper(...)` for the rest of that request but **not**
  `("x"):upper()`, because the string metatable still points at the untouched original.
- `getmetatable`, `rawset`, `dofile` and `loadfile` are not in scope. Each is a route back to the
  shared tables or the real globals; `setmetatable`, `rawget`, `rawequal` and `rawlen` are.
- `load(chunk)` defaults the chunk's environment to the request's, not to `_G`. Passing a fourth
  argument explicitly still wins.
- `package` carries `path`, `cpath`, `config`, `loaded`, `preload` and `loadlib`. `searchers` is
  absent because request `require` does not consult it.

### SQLite

```lua
local db = sqlite.open("myapp/data.db")   -- relative to the data dir; WAL mode, 5s busy timeout
db:execute("CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT)")
local r = db:execute("INSERT INTO notes (body) VALUES (?)", { "hello" })
-- r.changes, r.last_insert_rowid
local rows = db:query("SELECT * FROM notes WHERE id > ?", { 0 })
-- rows[1].id, rows[1].body — NULL↔nil, INTEGER↔integer, REAL↔number, TEXT/BLOB↔string
db:close()                                 -- optional; closes with the request anyway
```

Parameters always go through prepared-statement binding — string concatenation into SQL is
never needed, so injection is off the table.

**Connections are reused between requests.** SQLite opens lazily, so the first statement on a new
connection pays for the file open and schema load — around 220µs, against 0.7µs on a warm one. A
finished request parks its connection for the next one on the same worker thread, which measured
as roughly a 1ms saving on a 5ms Drive page. A connection is parked only if it can be handed on
cleanly: an open transaction is rolled back, `foreign_keys` is reset to its fresh-connection
default, and a connection carrying temp tables is discarded rather than reused. `db:close()`
parks it early. This shares no more than the database already shares — set
`--sqlite-idle-connections 0` to go back to a fresh connection every time. To bind SQL `NULL` use `sqlite.NULL`
(`{ id, folder or sqlite.NULL }`) — a bare `nil` would truncate the Lua array.

### Crypto

```lua
crypto.password_hash("hunter22")            -- argon2id PHC string
crypto.password_verify("hunter22", hash)    -- true/false
crypto.random_token(32)                     -- 32 OS-random bytes, hex-encoded (64 chars)
crypto.sha256("data")                       -- hex digest
crypto.constant_time_equals(a, b)           -- timing-safe comparison for tokens
```

### Subprocesses and files

```lua
local r = process.run{ "zip", "-rqX", "-1", out, ".",
                       cwd = staging, timeout = 600, capture = true }
-- r.started (did it launch), r.ok, r.code, r.timed_out, r.stdout, r.stderr

fs.mkdir("drive/tmp/stage")          -- relative to the data directory
fs.link("drive/files/ab12", "drive/tmp/stage/report.pdf")
```

`process.run` execs a program directly with an argument vector — **no shell** — so a filename full
of `;`, `$(...)` or quotes is one argument rather than syntax, and quoting stops being a concept
for callers. `started = false` means the program could not be launched at all, which is a different
answer from one that ran and failed, and is how you test for a tool's presence.

It is also bounded, which `os.execute` cannot be: the instruction hook cannot fire while Lua waits
on a child, so the `timeout` here (default 60s) is the only thing that can end a hung one. Captured
output is held in memory and capped at 8 MiB, with the pipes still drained past that so the child
never blocks on a full one. Pass `capture = false` when you only care about the exit status.

`fs.mkdir` and `fs.link` cover the two things Lua cannot do for itself, and are confined to the
data directory like `sqlite.open` and `send_file`. `fs.link` makes a hard link — copying no data —
and falls back to a real copy when the destination is on another filesystem.

### Background threads

A thread is just another Lua instance on its own OS thread — it owns its loop and timing:

```lua
-- jobs/worker.lua
while true do
    do_the_work()
    sleep(600)                 -- seconds; fractions allowed
end
```

```lua
local spawned, id = thread.spawn("worker", "jobs/worker.lua")  -- id identifies this run
thread.spawn("export", "jobs/export.lua", { folder = 42 })     -- reaches the script as `args`
thread.running("worker")     -- true while a thread of that name is alive
thread.status(id)            -- "running" | "finished" | "failed" | "cancelled" | nil
thread.join(id, 5)           -- wait up to 5s -> status, value
thread.kill(id)              -- ask a run to stop; true if it was still going
thread.id()                  -- inside a thread: its own run id; nil in a request
```

**Names and run ids are different things.** A *name* answers "is a worker of this kind alive?"
and is reusable: when a thread ends, the next spawn may take the name. A *run id* identifies one
execution and is never reused.

Status and results are keyed by the run id, which matters as soon as requests are concurrent. The
claim is atomic, so two requests racing on one name cannot both spawn — exactly one wins, and the
loser is handed the winner's id so it can still watch the same run. **When `spawned` is false your
`args` were ignored**, since the run already going has its own; check the flag if that matters.

The subtler reason ids exist is generational. A caller holding only a name could spawn, wait, then
read the status of a *later* run that reused the name in between, with no way to notice. An id
cannot be reused, so it always answers for the run you actually started.

**Arguments and results travel as JSON.** A Lua value belongs to the state that built it and
cannot cross into another one, so `args` is serialized by the spawner and rebuilt inside the
thread, and a return value makes the same trip back. For that same reason you cannot hand
`thread.spawn` a *function* — pass a script path and data. A return value that will not serialize
is logged and left empty rather than failing the run.

`thread.join` sleeps on a condition variable rather than polling. It returns `status, value` — the
return value when finished, the error text when failed, `nil` while still running, which is also
what a timed-out wait looks like. It defaults to 30s and is capped at 300s, because a join holds a
worker for its whole duration and the execution-timeout hook cannot fire inside it, exactly as
with `os.execute`. **For work that outlives a request, poll `thread.status` across requests rather
than joining.** A thread that joins itself is refused instead of left to block.

`thread.kill` is **cooperative**, because an OS thread cannot be stopped from outside. It raises
a flag that the target checks on its next instruction-hook firing, and `sleep` naps in 100ms
slices rather than one long stretch so a killed worker dies promptly instead of finishing its
ten-minute nap first. A thread parked in a blocking C call — `os.execute` above all — will not
notice until that call returns, the same limitation the execution timeout has. A `pcall` inside
the script cannot swallow it for long either: the flag stays raised, so the next hook firing
raises the error again. A killed run reports `"cancelled"` rather than `"failed"`, since a
deliberate stop is not a fault, and its name is freed like any other ending.

The registry is process state, not durable state: at most 256 finished runs are kept before the
oldest is dropped, and a restart loses all of it. Anything that must survive one belongs in
SQLite — which is also the answer when a worker needs a queue rather than a single argument.

`thread.spawn` is **idempotent by name**, so calling it from a template on every request is
a cheap "make sure my worker is alive" — a crashed worker gets resurrected by the next
request. `--thread jobs/worker.lua` does the same spawn at boot. Threads get the core API
(`server`, `sqlite`, `crypto`, `json`, `thread`, `log`; `print` goes to the log) but no
`request`/`response`; a thread ends (and frees its name) when its script returns or errors.

Limits: threads carry **their own**, separate from the per-request ones, because they are not
the same threat model. A request comes from an anonymous stranger at whatever rate they choose
and holds an HTTP worker while it runs; a thread is started by code you wrote, is idempotent by
name, and is capped at 64. So:

- **No execution deadline by default.** A worker legitimately computing for ten minutes is
  indistinguishable from a spin loop, and a runaway thread only burns a core — it holds no
  connection and cannot be multiplied by an attacker. Set `--thread-timeout-ms` if you want one;
  it then bounds each *awake stretch*, with `sleep()` resetting the deadline, so an eternal
  work-sleep loop is fine while a loop that never sleeps is killed.
- **A memory cap that stays** (`--thread-memory-limit-mb`, default 256), applied to the instance
  for its whole life. This one matters *more* for a thread, not less: a leaking request VM is
  bounded by the request, while a leaking long-lived thread grows until it takes the process with
  it. Hitting it kills only that thread and frees its name, so the next `thread.spawn` resurrects
  it — for a thread this is a supervisor, not a security control.

At most 64 named threads run at once.

### Helpers

```lua
print("Hello, ", name)            -- writes to the page, args joined by spaces
html_escape(user_input)           -- escapes & < > " '
json.encode(value)                -- Lua table -> JSON string (2nd arg true = pretty)
json.decode(text)                 -- JSON string -> Lua table
log.trace(...) log.debug(...) log.info(...) log.warn(...) log.error(...)
server.data_dir                   -- the configured data directory
server.version                    -- Red Crescent version
server.max_upload_size            -- limits, for showing in upload forms
server.max_upload_files
server.max_body_size
```

`<?lua= expr ?>` escapes for you, so `html_escape()` is only needed for HTML you assemble in Lua
and then emit with `print(...)` or `<?lua== ... ?>`:

```lua
-- escaping happens automatically here
<?lua= user.name ?>

-- but not here, so escape the parts yourself
print('<b>' .. html_escape(user.name) .. '</b>')
```

**Anything written through `<?lua== ... ?>` or `print(...)` is unescaped, and user input reaching
either one unescaped is an XSS hole.**

## Security Model

**Red Crescent speaks plain HTTP and has no TLS of its own. Without HTTPS terminating in
front of it, login credentials and session cookies cross the network in cleartext, readable
by anyone on the path.** Run it behind nginx, Apache or Caddy with a certificate. The default
bind of `127.0.0.1` is deliberate, so an unconfigured server is not exposed by accident.

Templates are **trusted code**, exactly like PHP files: they run with the full Lua standard library, including `os.execute` and `io`. Only deploy templates you wrote. What the server does protect against:

- **Runaway templates**: an execution timeout aborts infinite loops (a template-level `pcall` can't suppress it) and a memory limit stops runaway allocation. A watchdog thread owns the clock; nothing is installed in the VM until a deadline passes, so the guard costs nothing while a request is inside its budget. Caveat: it can't interrupt a blocking C call such as a hung `os.execute`.
- **Path traversal**: every path is canonicalized and must remain inside the serve directory (encoded `../` included). `sqlite.open` and `response.send_file` are likewise confined to the data (and serve) directories.
- **Source disclosure**: `.lua` files and dotfiles under the serve directory are never sent to a client — not as static files, and not through `response.send_file` either, so an app that passes a user-controlled path to it cannot be tricked into handing out its own modules or `rc_config.lua`. Symlinks don't help an attacker: paths are canonicalized before the check. The data directory is exempt from the source rule (a `.lua` file there is user content), and the server refuses to start with the data directory inside the serve directory.
- **Error leakage**: without `--dev`, error pages carry no details; specifics go to the server log only.
- **Request isolation**: each request runs in its own closed environment holding private copies
  of every library and API table, so nothing it writes — a global, `string.upper`, a module's
  table — can reach another request. Lua states are reused between requests (`--lua-pool`), and
  the environment, not the state's lifetime, is what isolates them. *Native C modules are the
  exception* (below): `dlopen` caches a library per process, so whatever one keeps in its own
  globals is shared by every request and every background thread. Enabling them therefore also
  turns pooling off. With C modules off — the default — the guarantee is unconditional.

### Native C modules

`require` loads `.lua` files only. Native Lua modules (`.so`) are refused outright unless the
operator names the directories they may be loaded from:

```bash
cargo run -- --c-module-dir /usr/lib/x86_64-linux-gnu/lua/5.4
```

```lua
-- rc_config.lua
c_module_dirs = { "/usr/lib/x86_64-linux-gnu/lua/5.4" },
```

Each directory contributes `<dir>/?.so` to `package.cpath`, so `require("socket.core")` looks for
`<dir>/socket/core.so`. Naming no directories — the default — leaves native modules disabled.

**This needs a build that supports it.** Red Crescent links Lua statically (mlua vendors it),
which leaves the Lua C API out of the binary's dynamic symbol table — a module would fail to load
with `undefined symbol: lua_gettop`. Exporting those symbols also defeats dead-code elimination
and costs about 70% binary size, so it is off by default:

```bash
cargo build --release --features c-modules
```

A binary built without the feature *refuses to start* when `c_module_dirs` is set and names that
command, so the mismatch surfaces at boot rather than as a cryptic symbol error on the first
`require`.

Modules must be built for **Lua 5.4**. Nothing extra ships alongside the server either way: the
binary has no `liblua` dependency of its own, and enabling C modules adds only the `.so` files you
actually use (on Debian, `apt install lua-zip lua-filesystem` and friends put them in the
directory named above).

Directories must exist at startup and must sit *outside* the serve directory: a `.so` below the
serve directory would also be downloadable as a static file, which is a worse version of the
source disclosure the server otherwise refuses. Both conditions are startup errors, not warnings.

**Enabling this switches every Lua state out of mlua's safe mode**, for requests and background
threads alike, and the server logs a warning at boot saying so. Three of the guarantees above
stop holding:

- The **execution timeout** cannot fire inside a C call — the same limitation `os.execute` has.
- The **memory limit** still bounds Lua's own allocations, but a module that calls `malloc`
  itself is invisible to it.
- **Request isolation** is no longer unconditional, for the `dlopen` reason given above.

A native module is trusted code in the strongest sense the server has: it runs as the server user
with none of the sandbox applying, and one that misbehaves across the Rust boundary can corrupt
state in ways no check here will catch. This is the same bargain PHP makes with its extensions —
load only modules you would trust with the whole process.

## Performance

- Lua executes on blocking threads (`web::block`), so async workers keep serving other requests while a template runs.
- Templates are compiled once and cached (keyed by mtime + size); disabled in `--dev`.
- Output goes through a Rust-side buffer — no quadratic string concatenation.
- Lua states are pooled per thread, which removes ~265 µs of setup per request: on the reference
  box a small page measured 8,554 rps with `--lua-pool false` against 26,188 with pooling on.
  Building a state per request stays available for C-module deployments and as an escape hatch.

## Development

```bash
cargo test          # unit + integration tests
cargo clippy        # lints
cargo run -- --dev  # local server with detailed errors and no caching
```

## Contributing

Contributions are welcome! Please open an issue or submit a pull request for any improvements or bug fixes.

## License

This project is licensed under the MIT License. See the LICENSE file for details.
