# TODO

Done since this list was written: inline handlers removed and a CSP enabled, app-level login
throttling, per-user storage quotas, the blunt TLS warning in the README, a backup script
(`deploy/backup-drive.sh`), opt-in native C modules (`--c-module-dir` /
`c_module_dirs`, behind the non-default `c-modules` build feature), `request.remote_addr`,
separate limits for background threads (`--thread-timeout-ms` / `--thread-memory-limit-mb`), and
a real thread model — arguments, run ids, `thread.status`, `thread.join`, `thread.id` and
`thread.kill` — and Drive's archive worker, which retired the synchronous build.
`panic = "abort"` is gone from both
profiles, and SQLite connections are now reused between requests
(`--sqlite-idle-connections`, default 2), worth about 1ms on a 5ms Drive page.
What remains:

Things deliberately deferred, with enough context to pick them up cold. Roughly ordered by
how much they'd matter in practice.

Several of the platform items came out of comparing the project against PHP, which is what it
invites comparison to. Only the gaps are listed — the places it comes out ahead (a fresh Lua
instance per request, `thread.spawn`, bind-only SQL, argon2 with no `md5()` beside it) need no
work.

## Platform (Red Crescent)

### Multi-site support
One instance serving several sites, each with its own bind address, serve directory and data
directory. Today it is one process per site — which is what `deploy/etc/systemd/system/drive.service`
assumes — and that becomes awkward as soon as there is a second app.

Open design questions worth settling before writing code:
- Several listeners in one process, or one listener with name-based (`Host` header) routing?
  Name-based routing is what nginx does and composes better with a reverse proxy; multiple
  binds are simpler and keep sites isolated at the socket level.
- Does `rc_config.lua` grow a `sites = { ... }` array, or does each site keep its own config
  file with a top-level file listing them?
- The data directory is currently a single sandbox handed to `sqlite.open`/`send_file`. With
  multiple sites it must become per-site, or one app could read another's database.
- Template cache, thread registry and the spool are process-global today; each would need to
  be keyed by site.

### Streaming responses
The rendered body is buffered whole and handed to actix in one piece
(`builder.body(rendered.body)` in `src/web.rs`), so nothing a template produces reaches the client
until it has finished. No server-sent events, no progress during a long operation, no generating a
large download without staging it to disk. `response.send_file` already streams, so the gap is only
in *generated* output.

**Design, decided:** a bounded channel, not a coroutine. The render closure gets a sender; the async
side spawns it rather than awaiting, and decides the response shape from the first message -- a
header commit means stream, no message before completion means build the response exactly as today.

The coroutine alternative was rejected on a fact rather than taste: `Lua` is `!Send`, so a coroutine
must be resumed on the thread that created it, and a blocking thread stays parked for the request
either way. Streaming buys time-to-first-byte and bounded memory, **not** concurrency. A channel
gets both without a resumption state machine or a third way to unwind alongside `exit()`/`redirect()`.

**`flush()` is the opt-in.** A template that never calls it must stay byte-identical to today, which
keeps every existing test and all of Drive on the current path. An auto-flush threshold was
considered and rejected: it silently commits headers mid-render, and Drive's listing writes ~500 KB
before it finishes, so that page's semantics would change under it. If a threshold is ever wanted,
it belongs behind a config key that defaults to off.

**First flush commits headers.** After it, `response.status`, header writes, `redirect()` and
`send_file()` cannot work -- PHP's "headers already sent". Raise, do not silently no-op.

Two properties to accept before starting, neither fixable:
- **A streamed response is not atomic.** An error after the first flush has no error page to send:
  the client already holds a 200 and a partial body. Log and truncate, so it looks incomplete rather
  than plausibly complete. PHP has the same hole.
- **A slow client parks a blocking thread.** Backpressure is correct and keeps memory flat, but the
  execution-timeout hook cannot fire during a blocked send -- same class as `os.execute`. Tie the
  send deadline to the request's remaining budget rather than adding a second knob.

Touches the core request path: `web.rs` (spawn, select on first message), `runtime.rs` (sender and
flush plumbing), `api.rs` (`flush()` and the post-commit guards), plus tests.
### Per-request fixed cost: the heavy API tables
Measured 2026-09-21 (release, in-process, avg of 2000): `Lua::new` 57us, `api::install` 64us,
teardown ~45us -- about 170us before a template runs. Template and module parsing are both cached
as bytecode now (see the two "bytecode cache" entries below), so what is left of the fixed cost is
`install`: `sqlite`, `crypto`, `fs`, `process` and `thread` are built on every request and most
pages touch none of them. Registering them behind an `__index` on the globals so they materialise
on first use is worth roughly 30-40us. The "VM pooling: no" verdict below still stands for the
`Lua::new` half: pooling is the only thing that removes it, and it trades a structural isolation
guarantee for it.

### `_out` should be backend-dependent
**Partly done 2026-09-21.** `_out` is now a raw `lua_CFunction` (`lua_out` in `src/api.rs`), fed by
a thread-local that `OutputScope` in `render` points at the current request's buffer. `_out_expr`,
`_out_raw` and `html_escape` are still mlua callbacks. The JIT/buffered variant below is untouched
and still waiting on the LuaJIT decision. Remaining below:

`_out` is an mlua callback today, ~105ns a call, and a 1000-row page makes ~8000 of them. Two
cheaper implementations, chosen by which Lua backend is built (same 60,048-byte page):

| `_out` implementation | lua54 | luajit |
| --- | --- | --- |
| mlua callback (today) | 1371 us | 926 us |
| raw `lua_CFunction` | **862 us** | 320 us |
| Lua closure buffering, flush at 64 KiB | 1457 us | **156 us** |

**Default (non-JIT): a raw `lua_CFunction`** via `Lua::create_c_function`, reading the string with
`lua_tolstring` and appending to a thread-local sink. 19-23ns a call against mlua's 105-118,
because it skips the upvalue lookup, `callback_error_ext`, the boxed closure and argument
marshalling. 1.6x on heavy pages for no other change. It is `unsafe`: preallocate the sink per-VM
at state creation so the write path never allocates and has no fallible branch, use
`UnsafeCell<Vec<u8>>` rather than `RefCell` (whose `borrow_mut` is itself the panic), and keep the
only allocation in the occasional flush where safe Rust and `try_reserve` apply. 

**JIT backends: `_out` as a Lua closure** that appends to a table and flushes to the raw sink every
N bytes. LuaJIT inlines it into the trace, so it beats even the 23ns call. Flush cost is negligible:
156us at 64 KiB against 156 unflushed, and still 164 at 1 KiB.

*Not settled.* That holds for escape-light pages. On an escape-heavy one the buffer loses to a raw
`_out` with fused escaping (568us vs 415us on LuaJIT), because the two cannot coexist -- see "Fuse
escape-and-write" below. Decide against a real template.

**The generated chunk does not change.** Both are just `_out(...)`; only its definition differs, so
`template.rs` stays as is.

Buffer size configurable. Subtract roughly **2x** the threshold from the effective Lua memory limit:
the pending pieces are strings the template allocated anyway, but the table's array part is ~8 bytes
per piece and the `table.concat` at flush allocates a new string of about the threshold size.

"Flush" here means Lua buffer -> Rust `Vec`, **not** Rust -> socket, so this does not conflict with
the streaming-responses design above. Needs the Lua backend to become a cargo feature of this crate.

### html_escape should be a raw C function too
**Not done yet.** Unlike `_out` it returns a value, so it needs an allocation (which can only
abort, not unwind) and a decision on the non-string case: today `html_escape(nil)` is a type
error, and a raw version either raises via `lua_error` or silently passes the value through --
the latter is unacceptable for a security-relevant function, since `html_escape(t)` returning a
table would reach `_out_raw` and be emitted unescaped. Design that before writing it.

**Keep it raw even after escape-by-default lands.** The marginal cost is near zero: the escaper is
already being written as a raw C function for `_out_expr`, so this is a second registration against
the same implementation, short-circuit fast path included.

The justification is forward-looking rather than measured, and should be read that way. Drive today
has only 3 non-inline uses (2 in `util.flash_html()`, neither in a loop). But escape-by-default
covers only `<?lua= ?>` *output sites*; anywhere an app builds HTML inside Lua -- a helper, a module
function, a loop assembling rows -- explicit `html_escape` is the only option. And the change
creates that pattern: `<?lua== ?>` paired with Lua-built HTML means every escape inside it is a
manual call. An app doing that in a loop would otherwise hit a ~202ns mlua callback per value.

Per-call sizing, which holds wherever it is called in bulk: as an mlua callback it costs ~202ns
against ~137ns raw, a fixed ~65ns saving. That is a large fraction of the cost for a short filename
and negligible for a large blob, so it qualifies because template values are usually short, not
because escaping is cheap.

The "hundreds to thousands of calls per listing" figure that originally justified this belongs to
`_out_expr` once escape-by-default lands, not to `html_escape`. Likewise the 25% measured on LuaJIT
(762us -> 568us on a 1000-row page) was the separate-call path that escape-by-default removes for
inline sites. Both are kept here as the sizing for any app that *does* call `html_escape` in bulk.

Taking and returning `mlua::String` instead of `String` is the smaller version of this fix and
still worth it if the raw C route is not taken: it skips the UTF-8 validation and one allocation.

### Escaping should short-circuit when nothing needs escaping
**Done 2026-09-21.** `needs_escaping` gates both `html_escape` and `_out_expr`; the Lua binding
hands back the same `mlua::String` when nothing needs escaping, so no allocation and no interning
on the common path. Original reasoning: Applies to
whichever function does the escaping -- `html_escape` today, `_out_expr` after escape-by-default. The current
implementation walks every string, builds a new `String`, and returns a fresh Lua string that gets
allocated and interned -- even for `report.pdf`, where the output is byte-identical to the input.
Scan first and return the argument untouched when it contains no `&<>`, and at nine values in ten
that is nine allocations and nine internings avoided.

Measured on a 1000-row page, three escaped cells per row, one value in ten actually containing a
special character:

| escape implementation | lua54 | luajit |
| --- | --- | --- |
| mlua callback, always copies (today) | 1618 us | 768 us |
| raw C, always copies | 1442 us | 572 us |
| raw C, short-circuiting | **1279 us** | **413 us** |

No unsafe required for the short-circuit itself, no ordering constraints, no backend dependency,
no template-compiler work. It composes with everything else here.

**It also dissolves the buffering-vs-fusion conflict below.** On luajit, buffered `_out` with
short-circuiting escape is 413us against fusion's 390us, a 6% gap, so the buffered path no longer
needs rescuing and the two designs stop competing.

### Escape by default, with `<?lua== expr ?>` for raw output
**Done 2026-09-21.** `_out_expr` escapes, `_out_raw` does not, `<?lua==` parses to
`Segment::RawExpr`, and `print` still writes unescaped. Drive, demos and fixtures migrated
(31 redundant `html_escape(` stripped, 22 sites moved to `<?lua==`), README updated, tests added
for both forms plus the deliberate double-escape of the old shape. Original design:

**Supersedes the escape-and-write peephole**, which existed only to recognise
`<?lua= html_escape(x) ?>` and fuse it. If `_out_expr` escapes unconditionally there is no shape
left to recognise: one call instead of two, no intermediate Lua string, and no compiler pattern
matching. Twig, Jinja, Django and Rails all landed here.

Today `<?lua= expr ?>` compiles to `_out_expr(expr)` and does **not** escape; the README puts the
burden on the author ("Always `html_escape()` user input"). In Drive only 28 of 157 inline sites
escape. "Remember to escape" is the classic XSS source, so the safety case is stronger than the
speed case -- though the speed case is real, measured on a 1000-row page with three escaped cells
per row:

| | lua54 | luajit |
| --- | --- | --- |
| separate escape (today) | 1618 us | 768 us |
| escaping inside the write | **903 us** | **390 us** |

(Those figures do not yet include the short-circuit fast path below, which should add a little
more by turning the no-escape-needed case into a straight memcpy.)

**Raw output: `<?lua== expr ?>`.** Rails' convention (`<%= %>` escapes, `<%== %>` does not).
Safe to claim because the parser branches on the single byte after `<?lua`, and `=` already means
"expression", so `<?lua== x ?>` currently compiles the source `= x` and is a **guaranteed Lua
syntax error**. No valid template can be relying on it, and it costs one extra byte of lookahead.

Two rejected alternatives, recorded so they are not revisited:

- `<?lua raw= expr ?>` **collides.** A space after `<?lua` already means "code block", and
  `raw = x` is valid Lua assigning to a global named `raw`. Adding the form would silently
  reinterpret such a block rather than erroring, and it needs keyword lookahead that must not
  match `<?lua rawcount = 1 ?>`.
- A `raw(x)` **function marker** (Twig's `|raw`, Jinja's `|safe`) needs no parser change but makes
  rawness a property of the *value* rather than the output site, so `<?lua= raw(a) .. b ?>` either
  fails on the concatenation or needs metamethods to survive it. Syntax applies to the whole
  expression result and cannot be half-applied.

`html_escape` stays as a Lua-visible function for building escaped strings mid-expression, but
becomes rare, and the README's "always escape" instruction is replaced by documenting `<?lua== ?>`
as the thing to grep for in a security review.

**Drive migration: 49 of 157 sites.** 28 drop their now-redundant `html_escape(` or they will
double-escape; ~21 need `<?lua== ?>`, and those name themselves -- `csrf_html` at 15 sites and
`util.flash_html()` at 6. The rest are ids, numbers and formatted dates that escaping leaves
byte-identical, and `share_url` at 5 sites gains correct `&amp;` encoding in attributes, which is
a fix rather than a regression.

### Measure the cost of building SQLite result tables
`sqlite:query` returns every row in one Lua table, so it is one callback per query and *not* a
callback hot path. But building that table means ~1 `create_table` and ~6 `Table::set` calls per
row from Rust, each with its own state lock and stack manipulation: ~7000 mlua table operations
for a 1000-row Drive listing. That is a different cost from callback overhead and a raw C function
does not address it. Unmeasured. Worth knowing before optimising anything else on that page.

### Native TLS (rustls)
So a standalone deployment needs no reverse proxy. Meets the "every web project needs it" bar;
nginx currently supplies it, which is a perfectly good answer for now.

### Other SQL backends
`sqlite.open` is the only database there is. SQLite's single writer is a real ceiling once an app
has concurrent writers, and it rules out putting the data on a different host from the app.
MySQL and Postgres are most of what people mean by "server-side language with a database", so
this shapes who will consider the project at all.

Two design questions, both worth settling before any code:
- One API shape (`db:query`/`db:execute` with a driver picked from a connection string) or a
  global per backend? One shape is far nicer to write against, but it has to paper over
  placeholder syntax (`$1` vs `?`), type mapping and NULL handling. `sqlite.NULL` already exists
  because a bare `nil` truncates a Lua array — that wart needs a generic answer, not a
  per-driver one.
- The confinement story does not carry over. `sqlite.open` is safe partly because it cannot
  escape the data directory; a connection string is an outbound network connection to anywhere,
  which is a materially different posture from anything the platform currently permits. Decide
  whether that is configuration (an allowlist of hosts) or simply accepted and documented.

### The execution timeout cannot interrupt a blocking C call
The README mentions this as a caveat; it belongs on this list because Drive is already standing
on it. `archive.lua` gives its subprocess a 600s timeout while `apps/drive/rc_config.lua` sets
the request timeout to 30s — and the archive wins, because the instruction hook cannot fire while
Lua is parked inside `os.execute`. PHP's `max_execution_time` has precisely the same hole on
Linux, so this is not a regression against PHP, but an app depending on it is depending on a gap
rather than on a guarantee, and a future change that narrows the gap would break it.

`process.run` now carries its own timeout, which covers the case that actually occurs in practice.
The general problem — a hung C call inside any built-in, or inside a native module now that
those can be enabled — is only solvable by running the Lua instance somewhere killable, which is a much larger change and probably not worth it.

### Multipart reading dispatches per chunk
`read_multipart` in `src/web.rs` calls `web::block` for every chunk actix hands it. Over a
network the chunks are small, so the count of thread-pool round trips climbs with transfer
size. This was suspected during a slow Tailscale upload but never proven — the CPU turned out
to be WireGuard's ChaCha20 in `tailscaled`, not us. **Measure first**: `ps -o time= -p <pid>`
across a 1 GiB upload. A few seconds of CPU per GiB is fine and needs no change; tens of
seconds would justify buffering chunks to ~1 MiB before touching the blocking pool.

### Align the rejection-drain budget with nginx
When an upload is refused mid-stream the server drains up to 5s / 128 MiB so the client can
still read the response (`REJECT_DRAIN_*` in `src/web.rs`). nginx solves the same problem with
`lingering_close` for up to 30s. Now that both sit in the same request path, it is worth
checking they don't work against each other.

## Drive

### No POSIX metadata is preserved
Uploads come back with the upload time and default permissions — Drive stores bytes only.
Harmless for documents, wrong for anything executable. Decide whether `files` should carry
`mode`/`mtime`, and whether the archive builder should restore them.

### Empty directories vanish
A browser's `webkitdirectory` upload only submits files, so a directory containing nothing
never reaches the server and is missing from a round-tripped tree. Only fixable client-side
(JavaScript enumerating the directory), so possibly just worth documenting.

## Operations

### Monitoring
Nothing watches whether the service is up or the certificate renewed. `systemd` restarts a
crash, and certbot's timer renews, but neither tells you when it stopped working.

## Settled, with numbers

Recorded so these are not reopened from intuition.

**Lua backend: stay on lua54.** LuaJIT builds and runs the whole codebase unchanged (no 5.4-only
syntax is used anywhere), but on real Drive listings it is 6.71ms vs 5.54 at 10 files, 17.78 vs
6.85 at 100, 22.13 vs 24.22 at 1000, and 75.27 vs 65.13 at 5000 -- no win. Microbenchmarks say
LuaJIT wins on loops past ~1000 iterations and is 16x faster on hot arithmetic, but a real request
is SQLite, `gsub` and buffer writes, all of which are C. The interpreter is not the bottleneck.

*Qualifier added 2026-09-21, measured.* That verdict holds for Drive pages and is wrong for pure
text generation. Same 60,048-byte page built four ways, plus a per-call mlua callback cost:

| backend | 20k short strings | 20k long | short/long | page, Lua buffer | page, via `_out` | hot loop | ns per mlua call |
| --- | --- | --- | --- | --- | --- | --- | --- |
| lua54 | 4688 | 3503 | **1.34** | 1135 us | 1340 us | 4216 us | 104 |
| luajit | 2430 | 2915 | 0.83 | **177 us** | 978 us | 279 us | 120 |
| luau | 3670 | 4043 | 0.91 | 984 us | 1415 us | 3300 us | 122 |
| lua51 | 11887 | 10515 | 1.13 | 1865 us | -- | 2939 us | -- |

Three things fall out. **5.4's short-string interning penalty is real and unique to it**: only lua54
costs *more* for strings short enough to intern (ratio 1.34) than for longer ones holding 3x the
data. **It is not 5.1's string design that saves LuaJIT** -- plain lua51 is 2.5x worse than lua54
at strings -- it is the JIT. And **the two changes only pay off together**: LuaJIT via the current
`_out` path is just 1.4x, but LuaJIT with a Lua-side buffer is 1340 -> 177 us, about **7.6x**, which
would put us ~4x ahead of PHP's 720us on the page where we currently lose 3.3x.

The mechanism is not what I first assumed. LuaJIT does **not** abort traces on these C calls --
`jit.attach` counted 9 started, 9 completed, 0 aborted for every variant including the `_out` one.
The cost is that an mlua callback is ~110ns on every backend and no JIT can remove it. On lua54
that hides inside work which is slow regardless; on LuaJIT, 8000 calls x 120ns = 960us against a
978us page, so the calls *are* the page. 


Luau and luau-jit are out on both counts: `set_global_hook` is `#[cfg(not(feature = "luau"))]` and
this code uses it in three places, `StdLib::IO` and `StdLib::PACKAGE` do not exist there against 58
`require(` calls, and luau-jit measured 5.1x lua54's VM creation for no warm gain on untyped code.

**VM pooling: no.** Creation is 55us against an 8.2ms request -- 0.7% -- and reuse would convert
"no state can leak between requests" from a structural guarantee into a discipline. A pooled VM
staying JIT-warm is the only real argument for it, and the numbers above remove that argument.

**Cold VM creation, for reference:** lua54 55us, luajit 79us, luau 104us, luau-jit 283us. Template
parsing is 46-90us cold but 1.6us on a cache hit, which is why there is nothing to overlap it with.

**SQLite connection reuse: done, measured.** Parking a connection for the next request on the
same worker thread took a 50-file Drive listing from a 5.413ms median to 4.425ms, and 4.129 to
3.581 on a second alternating run, with a tighter p75 both times. A connection is parked only if
it can be handed on cleanly -- open transaction rolled back, `foreign_keys` reset, temp tables
mean discard. The rollback is covered by a test that was checked to fail without it.

**Drive's listing markup: done, measured.** Rows carried a `<details>` block with rename and move
forms, and the move form repeated a `<select>` of every folder the user owns -- quadratic in
folders. The forms now live in one dialog per page, rows carry a `Manage` link, and
`manage.lhtml` is the no-JavaScript path. Page bytes: 10 files 25,079 -> 9,114; 100 files
223,267 -> 49,261; 1000 files 2,211,372 -> 456,065; 5000 files 11,075,373 -> 2,288,066. Adding 20
folders to a 5000-file listing now costs 20 KB rather than megabytes. Row actions then became
icons, which costs about 11% back (the `title` + `aria-label` pair), leaving 1000 files at
506,221 bytes. Folders gained rename and move at the same time -- they previously had neither --
with `is_within` guarding against a move into a folder's own subtree.

**Trimming the standard libraries: no, measured.** Opening fewer libraries in `new_lua` looked
like the one lever on VM creation that does not involve pooling. It is not. A state with
`StdLib::NONE` still costs 19.1us to create and drop, against 56.6us for `ALL_SAFE`, so the
libraries are only ~37us of it and the floor stays high whatever you drop. Of the seven lua54
libraries, `coroutine` is the only one nothing uses -- worth 2-4us. `io` is used by Drive's
`lib/archive.lua` and `jobs/cleanup.lua`, `os.date`/`os.time`/`os.remove` appear 46 times across
demos and apps, `package` is what `require` runs on, and string/table/math are not negotiable. So
the reachable saving is a few microseconds out of a 171us request, in exchange for a breaking
change to the Lua surface. Not worth it.

**Where a request actually goes (2026-09-21, release, `api.lhtml`, avg of 3000).** Measured by
instrumenting each phase of `render` rather than by differencing, which is what made an earlier
pass misattribute the teardown:

| phase | us | share |
| --- | --- | --- |
| `Lua::new` | 66.1 | 39% |
| `api::install` | 45.0 | 26% |
| `drop(lua)` | 24.5 | 14% |
| execute template | 22.1 | 13% |
| load bytecode | 7.5 | 4% |
| cache lookup (a stat) | 3.7 | 2% |
| response extraction | 2.0 | 1% |
| memory limit + state + hook | 0.2 | 0% |
| **total** | **171.1** | |

Creating and dropping the VM is 90.6us, 53% of the request and more than everything else combined.
Outside the VM, install and the template itself there is only ~13us left, so there is no cheap win
in the plumbing. `include()` costs 6.71us per call (a canonicalize, a stat, the map lookup, loading
the bytecode into this state, then the call) and nothing is memoized per request, so a partial
included in a loop pays it every iteration -- 101 includes measured 819us against 149us for one.
Drive includes only a header and footer per page, so this costs it ~13us; it would only be worth a
`HashMap<PathBuf, Function>` on `RenderState` if a page ever includes per row.

**Module bytecode cache: done, measured.** `require` was wired to Lua's stock searcher through
`package.path`, and since every request gets a fresh state with an empty `package.loaded`, each one
re-opened, re-read and re-parsed every module it used -- 62us for `demos/utils.lua` on a cold state.
`package.searchers[2]` is now a Rust function (`search_lua_module` in `src/api.rs`) over the same
cache the templates use, so a module is parsed once per process and later requests load its
bytecode. `/` (which does `require("utils")`) went 6,884 -> 7,498 rps at 64 connections; endpoints
that require nothing are unchanged. The C searcher at slot 3 is untouched, so `--c-module-dir`
still works -- covered by `a_real_system_c_module_loads` under `--features c-modules`.

The stock contract is preserved deliberately: the same `?.lua` then `?/init.lua` order, dots
becoming directory separators, a loader plus its filename on success, and a `no file '...'` line
per path tried on failure so `require`'s own "module not found" message stays as useful as before.

It also closed a hole. The stock searcher resolves through symlinks, so a link inside the serve
directory pointing outside it was loadable as a module; the replacement canonicalizes and checks
the result against the serve directory, which is the rule `include()` already applied.
`require_refuses_a_module_symlinked_out_of_the_serve_dir` covers it and was checked to fail
without the check.

**Template bytecode cache: done, measured.** `TemplateCache` stored generated Lua *source* and
re-parsed it on every request. It now dumps the compiled chunk on first use (`OnceLock<Vec<u8>>` on
the cached entry, filled by whichever request compiles it first) and later requests load that.
Per-request chunk load: `api.lhtml` 16.8us -> 3.4us, `index.lhtml` 23.2 -> 3.1, `demo.lhtml` 212.7
-> 24.7; the one-time dump costs 1.4-6us. End to end at 64 connections: `/api.lhtml` 13,623 ->
15,833 rps, `/demo.lhtml` 4,200 -> 5,050, `/` 6,641 -> 6,745. The gain scales with template size,
so small endpoints barely move.

Three things that are easy to get wrong here. Bytecode carries its own chunk name, so `set_name` is
a no-op when loading binary and the name has to be set before the dump. A dump made on one VM loads
into any other VM on any thread and rebinds `_ENV` to that VM's globals, which is what makes one
dump serve every request.

And **dumps keep their debug info** — `dump(false)`, deliberately, in every mode. Stripping was
tried and removed: it saves 120B on `api.lhtml` and 1549B on the 20KB `demo.lhtml`, with a
load-time difference inside the noise (twice measured negative), and in exchange it destroys error
locations. Because only the first request after a restart compiles from source, a stripped build
logs the same failing template two different ways — `boom.lhtml:3: attempt to index a nil value
(local 't')` once, then `?:-1: attempt to index a nil value` forever after. The log carries that
detail outside dev mode too (`render_error_response` logs in full while the browser gets a generic
page), so it is exactly what a production bug report is read from. Not worth 1.5KB.

**Front-controller routing: done.** `fallback = "app.lhtml"` in `rc_config.lua` (or `--fallback` /
`RC_FALLBACK`) renders that template whenever a path resolves to no file, with the original path
in `request.path`. Real files still win, traversal is still a 403 before the fallback is reached,
and the template must be `.lhtml` inside the serve directory -- resolved once at startup, so a
typo is a boot error rather than a 404 page that misbehaves later. `.lhtml` is required because a
static file cannot set its own status and would answer every unresolved path with a 200; the
fallback defaults to 200 and owns its statuses, as a front controller should. Route *matching*
stays in Lua: a table of patterns beats anything a config file could express. This leaves
`--bind 0.0.0.0` and native TLS as what still separates a standalone deployment from one behind
nginx.

Drive uses it for share links only: `/s/<token>` instead of `/s.lhtml?t=<token>`, routed by
`apps/drive/app.lhtml`, which also answers every other unresolved path with Drive's own 404
rather than the server's bare error page. `deploy/drive.eicher.cc.adjusted` proxies `location /`
wholesale, so no nginx change was needed. Old `?t=` links were not kept working -- deliberate,
and the reason the deployed `deploy/var-www-drive/app` copy has to be refreshed in step.

Taking the rest of Drive resource-style (`/folder/3`, `/file/7/manage`, `/archive/2.json`) was
built and then backed out: it works, but it churns every link, form, redirect and test in the app
for a cosmetic gain, and the routing table plus a `lib/urls.lua` of builders is more machinery
than Drive earns today. Worth revisiting only if Drive grows pages whose query strings actually
get unwieldy.

**Subprocesses and the shell: done.** `process.run{argv}` execs directly with no shell and its own
timeout; `fs.mkdir`/`fs.link` cover what Lua cannot do itself, confined to the data directory.
Drive has no `os.execute`, `io.popen` or hand-rolled quoting left anywhere -- `shq()` is gone, and
so is the optional dependency on coreutils `timeout(1)`, since process.run enforces its own.
Archive staging was the last holdout: one process per file would have cost ~35s for 20000 files at
~1.7ms a call, so it moved to fs calls instead, which is both safer and faster than the generated
shell script it replaced.
