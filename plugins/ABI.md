# The Red Crescent Plugin ABI

Red Crescent must be expandable with plugins as an alternative to C modules. The reason for that is
that such modules are loaded and ready globally, and under direct control of the server itself.

**Status: prototype.** The server side is in `src/plugins/`, the header in `rc_plugin.h`, and the
Rust SDK with its `#[plugin]` macro in `sdk/`. `examples/sqlite/` is the built-in `sqlite` module
rebuilt as a plugin, API for API, as the proof of concept; `tests/sqlite_plugin.rs` holds it to the
built-in's own fixtures, and `examples/bench_plugin.rs` times the two against each other.
`tests/probe/` is the plugin `tests/plugins.rs` exercises the whole ABI with. See "Cost" below.

A plugin is a native library, `.so` on Linux and `.dll` on Windows, built for the platform it runs
on and loaded by the server at startup. It talks to the server through a small C interface, even
when written in Rust, because C is the only stable ABI both sides can rely on. The interface never
exposes the Lua state (`lua_State`). A plugin sees values and **references**, and everything it
does to Lua goes through the host. That is what separates a plugin from a C module:

| | C module (`--c-module-dir`) | Plugin |
| --- | --- | --- |
| Loaded | by `require`, per state | once, at startup |
| Sees | the raw `lua_State` | values and references |
| State pooling | turned off | stays on |
| Tied to | one Lua version, a `c-modules` build | the ABI version only |
| Lua errors | `longjmp` through the module | return codes |

## The Interface

The whole interface is one header, [`rc_plugin.h`](rc_plugin.h), which is authoritative; the
excerpts below follow it. All strings are UTF-8 with an explicit length
unless the name ends in `_cstr`. Every struct of function pointers starts with its own `size`, so
the host can append entries without breaking plugins built against an older header. A plugin
checks `size` before it uses an entry newer than the ABI version it targets.

### Values

```c
#include <stddef.h>
#include <stdint.h>

#define RC_PLUGIN_ABI 1

typedef enum {
    RC_OK        = 0,
    RC_ERROR     = 1,   /* the plugin failed; the message was set with host->error */
    RC_PROPAGATE = 2,   /* a host call failed; return this unchanged */
} RcStatus;

typedef enum {
    RC_NIL, RC_BOOL, RC_INT, RC_NUMBER, RC_STRING,
    RC_TABLE,           /* reference */
    RC_FUNCTION,        /* reference */
    RC_HANDLE,          /* an object of one of this plugin's handle types; another's is RC_OTHER */
    RC_OTHER,           /* reference to anything else: a coroutine, a foreign userdata */
    RC_NULL,            /* the null sentinel: sqlite.NULL, json.decode("null") */
} RcType;

/* A Lua table, function or other value, owned by Lua. Only usable through the host. */
typedef struct { uint64_t id; } RcRef;

typedef struct {
    RcType type;
    union {
        int b;
        int64_t i;
        double n;
        struct { const char *ptr; size_t len; } s;      /* binary-safe, borrowed */
        RcRef ref;
        struct { uint32_t type_id; void *ptr; } handle;
    } as;
} RcValue;
```

### The plugin's view of one call

```c
typedef struct RcHost RcHost;

typedef struct RcCall {
    const RcHost *host;
    const RcValue *args;
    size_t nargs;
    void *internal;     /* the host's; do not touch */
} RcCall;

/* Every function and method a plugin registers has this shape. */
typedef RcStatus (*RcFunction)(RcCall *call, void *userdata);
```

### `RcPluginContext`: what the plugin learns at install

```c
typedef struct {
    uint32_t size;
    uint32_t abi;                   /* the host's RC_PLUGIN_ABI */
    const char *server_version_cstr;
    const char *name_cstr;          /* the plugin's key in rc_config.lua: its Lua table name */
    const char *serve_dir_cstr;
    const char *data_dir_cstr;
    const char *options_json_cstr;  /* the plugin's `options` from rc_config.lua, as JSON, or "{}" */
    const RcHost *host;             /* valid for the life of the process */
} RcPluginContext;
```

### `RcRegistryCOM`: registering functions and handle types

COM-style: each entry takes the struct itself as its first argument.

```c
enum {
    RC_FN_BLOCKING = 1 << 0,    /* may wait (network, disk): keeps the template off the inline path */
};

typedef struct {
    const char *name_cstr;
    RcFunction fn;
    uint32_t flags;
} RcMethod;

typedef struct {
    const char *name_cstr;      /* shown by tostring() and in errors */
    const RcMethod *methods;    /* obj:method(...) passes the object as args[0] */
    size_t nmethods;
    void (*destroy)(void *ptr); /* runs when Lua collects the object; must not call the host */
    void *userdata;             /* handed to every method */
} RcHandleType;

typedef struct RcRegistryCOM RcRegistryCOM;
struct RcRegistryCOM {
    uint32_t size;
    /* name.fn: installs `fn` as a field of the plugin's table. */
    RcStatus (*function)(RcRegistryCOM *self, const char *name_cstr,
                         RcFunction fn, void *userdata, uint32_t flags);
    /* Declares a handle type; *type_id is what push_handle takes and RC_HANDLE carries:
       0, 1, ... in registration order, the same on every load. */
    RcStatus (*handle_type)(RcRegistryCOM *self, const RcHandleType *type, uint32_t *type_id);
    /* name = value in the plugin's table: nil, null, a boolean, number or string (copied). */
    RcStatus (*constant)(RcRegistryCOM *self, const char *name_cstr, const RcValue *value);
};
```

### `RcHost`: what a function can do while it runs

Each entry takes the current `RcCall`, and every entry that can fail returns an `RcStatus`.

```c
struct RcHost {
    uint32_t size;

    /* Results and errors */
    RcStatus (*push)(RcCall *call, const RcValue *value);               /* append a return value */
    RcStatus (*push_handle)(RcCall *call, uint32_t type_id, void *ptr); /* RC_OK: Lua owns ptr */
    RcStatus (*error)(RcCall *call, const char *msg, size_t len);       /* then return RC_ERROR */

    /* Output, into the page being rendered; unescaped, like print() */
    RcStatus (*write)(RcCall *call, const char *bytes, size_t len);

    /* Functions */
    RcStatus (*call)(RcCall *call, RcRef fn, const RcValue *args, size_t nargs,
                     RcValue *results, size_t max_results, size_t *nresults);

    /* Tables */
    RcStatus (*new_table)(RcCall *call, RcRef *out);
    RcStatus (*table_get)(RcCall *call, RcRef table, const RcValue *key, RcValue *out);
    RcStatus (*table_set)(RcCall *call, RcRef table, const RcValue *key, const RcValue *value);
    /* Start with key = nil; a nil key back means the end. */
    RcStatus (*table_next)(RcCall *call, RcRef table, RcValue *key, RcValue *value);
    RcStatus (*table_len)(RcCall *call, RcRef table, size_t *out);

    /* Lifetime */
    RcStatus (*retain)(RcCall *call, RcRef ref);    /* keep for the rest of the request */
    RcStatus (*release)(RcCall *call, RcRef ref);

    /* Logging, same sink as log.info() and friends; level 0-3 = debug..error */
    void (*log)(int level, const char *msg, size_t len);
};
```

### The exports

A library must export two functions:

```c
/* Must exist for RC to load the plugin: the RC_PLUGIN_ABI it was built against. */
uint32_t rc_plugin_abi(void);

/* To install the plugin into the registry. Called at startup, once per server. */
RcStatus rc_plugin_install(const RcPluginContext *ctx, RcRegistryCOM *reg);
```

`rc_plugin_abi` replaces the `is_rc_plugin` marker from the first sketch. It proves the library is
a plugin *and* says which interface it expects, so a mismatch is refused at boot rather than
crashing on the first call.

`rc_plugin_install` only **records**: the names, function pointers, flags, handle types and
constants. It never sees a Lua state. The host builds the Lua wrappers from that record into every
state it creates, including pooled states and background threads, so install cost is paid once,
not per request.

**One library can be installed more than once in a process**, each time with its own context: the
operating system loads a library once, but every server in the process installs it, and the test
suite runs many servers in one process. Globals are shared between those installs. State that
belongs to an install, such as the data directory, goes in the `userdata` of the functions and
handle types it registers, which is what `userdata` is for.

## Passing Lua values

Scalars (nil, booleans, numbers, strings) are copied or borrowed. **Tables and functions are
passed by reference**, so a plugin can take a callback or fill a table in place:

```lua
-- template
pg.each(conn, "SELECT id, name FROM users", function(row)
    print(row.name)
end)
```

```c
/* inside pg.each: for every row... */
RcValue row;                                      /* built with new_table + table_set */
size_t n;
RcStatus st = call->host->call(call, callback, &row, 1, NULL, 0, &n);
if (st != RC_OK) return st;                       /* error, exit() or timeout in the callback */
```

The rules that keep this safe:

- **Borrowed means until return.** String pointers in `args` and in `call` results stay valid until
  the plugin's function returns. Anything longer needs a copy.
- **A reference is valid for the current call.** `retain` extends it to the end of the request.
  The host invalidates all references when the request ends, because a pooled state serves the
  next request next and a function from this one must not be callable from it. A stale reference,
  or one used from another thread, gets `RC_ERROR` and never crashes.
- **Errors never unwind through the plugin.** Lua runs in protected mode: an error, an `exit()` or
  `redirect()`, or the time limit hitting inside a callback all come back from `call` as
  `RC_PROPAGATE`. The plugin cleans up and returns that status, and the host re-raises the
  original, so `exit()` inside a callback still behaves like `exit()`.
- **Results go through `push`.** The host copies each value when it is pushed, so neither side ever
  frees memory the other allocated. On Windows the two may use different C runtimes.
- **The sandbox still holds.** A plugin reaches only what the template hands it, never the globals.
  Changing a table it was given is the same as the template changing it, and the reset between
  requests already handles that.

### Handles

A handle is a plugin-owned object Lua holds, for example a database connection:

```lua
local conn = pg.connect("postgres://…")   -- push_handle
local rows = conn:query("SELECT 1")        -- a method from the handle type
```

`push_handle` hands `ptr` to Lua only when it returns `RC_OK`; on failure the plugin still owns it.
`destroy` runs when Lua collects the object. That can happen in a later request on the same thread
or when a state is retired, so it must not call the host. Creating a handle flags the request for
a full collection before its pooled state is reused, so a connection closes promptly rather than
whenever the collector gets to it. **State that should outlive a request, such as a connection
pool, belongs in the plugin's own globals, not in a handle.**

All handles share one metatable, whose `__index` is a plain table of every method name of every
handle type, so `obj:name()` costs a table lookup rather than a call into the host. A name that two
types share picks its function by the object's type. The visible side effect: `obj.name` is a
function, not nil, whenever any loaded handle type has a method of that name.

## Cost

### SQLite, built-in against plugin

The question this prototype had to answer: does a plugin make SQLite slower than the module
compiled into the server? `examples/bench_plugin.rs` renders the same pages through both, on
identical databases with the same connection reuse, in alternating rounds. Medians of three runs on
one machine (absolute numbers do not carry between machines; the ratio is the result):

| Page | Built-in | Plugin | Plugin / built-in |
| --- | --- | --- | --- |
| open, query 1 row | 76 µs | 80 µs | 1.04x |
| query 100 rows | 324 µs | 352 µs | 1.09x |
| query 1000 rows | 2345 µs | 2546 µs | 1.10x |
| 100 `execute`, no parameters | 855 µs | 953 µs | 1.10x |
| 100 `execute` with 2 parameters | 535 µs | 631 µs | 1.18x |

SQLite's own work costs the same on both sides. The difference is crossing: about 1 µs per method
call and about 0.25 µs per row returned (six host calls: a table and its fields). A typical page
pays a few percent; a page that loops over hundreds of statements pays up to a fifth.

The first measurement had `execute` at 1.4x. Half of that was the method lookup, then a host
function per `obj:name`; it is now a table (see "Handles").

### Where the rest goes

Measured earlier on the probe, release build, one request timing each operation in a loop:

| Operation | Plugin | Comparison |
| --- | --- | --- |
| Call, no arguments | `probe.noop()` 55 ns | Lua closure 15 ns; built-in `html_escape("x")` 98 ns |
| One string in, one out | `probe.echo("x")` 287 ns | `html_escape("x")` 98 ns |
| Four integers in and out | `probe.echo(1, 2, 3, 4)` 271 ns | |
| Calling back into Lua | `probe.apply(f, 1)` 377 ns | Lua `f(1)` 16 ns |
| Walking a table, per element | `probe.each(t, f)` 407 ns | Lua `pairs` loop 40 ns |
| Reading a table, per element | `probe.sum(t)` 79 ns | Lua loop 10 ns |

A bare call is cheaper than a built-in. Every value that crosses, and every host call, costs more
than it should. Profiled, on the probe and again on SQLite, that cost is the host's use of mlua's
general-purpose API, not the ABI:

- **Per call:** mlua's callback machinery, and `malloc`/`free` for the argument, result and
  reference vectors. Each value is also moved into mlua's reference store and back
  (`lua_xmove`, `lua_rotate`, `ValueRef::drop` in the profile).
- **Per host call:** each table access is a protected mlua call, string keys are re-created
  (`luaS_newlstr`), and every reference is cloned on use. `table_next` also fetches the real
  `next` from the registry by name every time.

The planned fix is a raw wrapper: a Lua C function that reads arguments off the stack into a
stack-allocated array, references that are stack slots rather than cloned values, and host calls
that use the Lua C API under one protected call each instead of mlua's reference store. Until
then, plugins suit coarse calls (render this, query that) better than per-element traffic.

## Loading

```lua
-- rc_config.lua
plugins = {
    hostname = { path = "/opt/red-crescent/plugins/hostname" },
    pg       = { path = "/opt/red-crescent/plugins/pg", options = { max_connections = 8 } },
}
```

- **The key is the Lua name**: `hostname` becomes a table in every request's environment and is
  also reachable with `require("hostname")`. A key that clashes with a built-in (`json`, `sqlite`,
  …) is refused at startup, which is why the SQLite plugin loads as `sqlite3` next to the built-in.
- **`path` has no extension**, so one config works on every platform. The host tries the path as
  written, then `lib<name>.so` on Linux and `<name>.dll` on Windows in the same directory. Relative
  paths resolve from the config file's directory.
- **A plugin inside the serve directory is refused at startup.** The static file server would
  otherwise hand the binary to anyone who asks for it, the same reason a data directory inside the
  serve directory is refused.
- **Plugins load once and are never unloaded.** Unloading native code that may have spawned threads
  or registered callbacks is not safely possible.
- **Failures stop the server at boot**: a missing file, a missing export, an ABI mismatch, or a
  non-`RC_OK` install, each named in the error.

## What the host cannot protect against

A plugin is native code in the server's process, so:

- **Only trusted code belongs here.** It has full access, and a crash in a plugin brings the whole
  server down.
- **The time limit cannot interrupt it.** The watchdog cannot stop native code, just as with
  `os.execute`. It can only stop Lua code a plugin calls back into. Register anything that can
  wait with `RC_FN_BLOCKING`.
- **Functions run concurrently.** Several render threads call the same function at once, each with
  its own `RcCall`, so plugin globals must be thread-safe. Handles and references never cross
  threads.
- **Memory is not counted.** The per-request Lua memory limit sees only Lua allocations.
- **A panic must not cross the boundary.** A panic reaching an `extern "C"` function aborts the
  process. The SDK catches them and raises a Lua error instead; hand-written entry points must
  do the same.
- **Two copies of one C library do not know about each other.** The SQLite plugin links its own
  SQLite next to the server's. SQLite's POSIX locks are per process, so if both copies open the
  same database file, one closing it drops locks the other relies on, which can corrupt the
  database. A plugin replacing a built-in must not run beside it on the same files.

## Writing a plugin in Rust

The SDK's `#[plugin]` turns a module of ordinary Rust functions into a plugin. It generates both
exports and a C entry point per function, converts arguments and results, turns `Err` and panics
into Lua errors, and keeps install state per install. The rules are on `#[plugin]` in
`sdk/macros/src/lib.rs`; in short:

- `#[function]` makes a `fn` callable from Lua, `#[function(blocking)]` flags it, and
  `#[function(raw)]` registers a hand-written `RcFunction` unchanged.
- `#[install] fn(&Context) -> Result<State>` runs at install; a function taking `&State` gets it.
- `#[constant] const NAME: ...` puts a string, number, boolean or `Value::Null` in the table.
- `#[handle] impl Type { ... }` makes `Type` a Lua object with its `&self` methods. Returning a
  `Type` hands it to Lua, and Lua drops it once collected.
- A parameter `&Call` is the running call; every other parameter is a Lua argument, converted
  with `FromLua`, and the return value with `IntoLua`. `&str`, numbers, `Option`, `Table`,
  `Function`, `Value`, `Vec` and maps all convert; borrowed values live as long as the call.

The hostname example, whole: `hostname.get()` returns the machine's name, and `hostname.print()`
writes it straight into the page.

```toml
# plugins/examples/hostname/Cargo.toml
[package]
name = "hostname"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]     # libhostname.so on Linux, hostname.dll on Windows

[dependencies]
gethostname = "1"
red-crescent-plugin = { path = "../../sdk" }
```

```rust
// plugins/examples/hostname/src/lib.rs
use red_crescent_plugin::prelude::*;

#[plugin]
mod hostname {
    use super::*;

    /// hostname.get() -> string
    #[function]
    fn get() -> String {
        gethostname::gethostname().to_string_lossy().into_owned()
    }

    /// hostname.print(): writes the name into the page, unescaped like print().
    #[function]
    fn print(call: &Call) -> Result<()> {
        call.write(get())
    }
}
```

Build it, name it in the config, and use it:

```bash
cargo build --release -p hostname
cp target/release/libhostname.so /opt/red-crescent/plugins/    # hostname.dll on Windows
```

```lua
-- rc_config.lua
plugins = { hostname = { path = "/opt/red-crescent/plugins/hostname" } }
```

```lua
<p>Served by <?lua= hostname.get() ?></p>     <!-- escaped, like any <?lua= ?> -->
<p>Served by <?lua hostname.print() ?></p>    <!-- raw -->
```

`examples/sqlite/src/lib.rs` shows the rest: install state from `options`, a handle type with
methods, a constant, tables built row by row, and errors from `rusqlite` passed on with `?`. A
plugin in C includes `rc_plugin.h` and exports the two functions itself; `sdk/src/ffi.rs` is that
header as Rust, and a test compiles the header to check the two agree.

## Decisions

- **No implicit `request` or `response`.** A plugin sees only what a template passes it. There is
  no `host->request(call)`; a template that wants a plugin to read the request passes `request`
  as an argument, which arrives as a table reference.
- **No yielding across a plugin.** A callback that yields a coroutine while a plugin's native
  frames are on the stack fails, as Lua's own C API does for a C function without a continuation.
  In the prototype the refusal is Lua's own "attempt to yield across a C-call boundary" error, so
  it reaches the plugin as `RC_PROPAGATE` like any other error in a callback, rather than as the
  `RC_ERROR` first planned; the plugin returns it and the template sees Lua's message.
- **ABI evolution.** Appending fields to the end of a struct is covered by its `size` and stays
  within the current version. A breaking change bumps `RC_PLUGIN_ABI`; whether the host keeps
  supporting the previous version for a while or refuses it is decided at that bump.
