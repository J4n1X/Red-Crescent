# TODO

Done since this list was written: inline handlers removed and a CSP enabled, app-level login
throttling, per-user storage quotas, the blunt TLS warning in the README, a backup script
(`deploy/backup-drive.sh`), opt-in native C modules (`--c-module-dir` /
`c_module_dirs`, behind the non-default `c-modules` build feature), `request.remote_addr`,
separate limits for background threads (`--thread-timeout-ms` / `--thread-memory-limit-mb`), and
a real thread model — arguments, run ids, `thread.status`, `thread.join`, `thread.id` and
`thread.kill` — and Drive's archive worker, which retired the synchronous build.
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
The rendered body is buffered whole and handed to actix in one piece (`builder.body(rendered.body)`
in `src/web.rs`), so nothing a template produces reaches the client until it has finished. That
rules out server-sent events, progress output during a long operation, and generating a large
download without staging it on disk first — all of which PHP does with `flush()`. It is also why
the Drive archive item below has to be solved with a worker and a polling page instead of simply
streaming the archive out as it is built.

Not a small change: Lua runs to completion inside a single `web::block` call, so streaming needs
either a channel the render loop writes into while the async side forwards chunks, or a Lua
coroutine that yields at flush points. Worth scoping properly before promising it. Note that
`response.send_file` already streams (it hands off to `actix_files::NamedFile`), so the gap is
only in *generated* output.

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

### A safe way to run a subprocess
C modules are opt-in and off by default, so shelling out remains the universal escape hatch for
any deployment that leaves them off, and Drive already leans on it in two places: `apps/drive/lib/archive.lua` runs `zip`/`tar` through `os.execute`, and
`apps/drive/jobs/cleanup.lua` reads `find` through `io.popen`. Both hand-roll their quoting
(`shq()` — wrap in single quotes, escape embedded ones) and both pass `--` so a name starting
with a dash cannot be read as an option. The code is careful and, as far as I can tell, correct.
It is also user-controlled data reaching a shell on every call, and Lua has no `escapeshellarg`
to fall back on.

The fix that removes the class of bug rather than one instance: a `process.run{argv}` that execs
a program directly with an argument vector and no shell in between, returning exit status and
captured output. Quoting then stops existing as a concept for callers. Open questions: does it
take a timeout of its own (it should — see the next item), is the child killed when the request
ends, and is the program an absolute path or looked up on `PATH`?

### The execution timeout cannot interrupt a blocking C call
The README mentions this as a caveat; it belongs on this list because Drive is already standing
on it. `archive.lua` gives its subprocess a 600s timeout while `apps/drive/rc_config.lua` sets
the request timeout to 30s — and the archive wins, because the instruction hook cannot fire while
Lua is parked inside `os.execute`. PHP's `max_execution_time` has precisely the same hole on
Linux, so this is not a regression against PHP, but an app depending on it is depending on a gap
rather than on a guarantee, and a future change that narrows the gap would break it.

A `process.run` with a timeout of its own (previous item) covers the case that actually occurs.
The general problem — a hung C call inside any built-in, or inside a native module now that
those can be enabled — is only solvable by running the Lua instance somewhere killable, which is a much larger change and probably not worth it.

### No routing without a rewrite in front
Path resolution is the filesystem and nothing else (`src/web.rs`): the decoded URL path is joined
to the serve directory, a directory gets `--index` appended, and anything that does not resolve
to a real file is a 404. There is no PATH_INFO, no rewrite, and no way to say "everything under
`/api/` goes to one template". A front-controller app works only because nginx can rewrite in
front of it — which is exactly PHP's position too, so this is a tie right up until the server is
meant to run standalone. That is the same day `--bind 0.0.0.0` and native TLS start mattering,
which is why these three belong together.

Cheapest useful version: an optional `fallback = "app.lhtml"` in `rc_config.lua`, served whenever
the path does not resolve to a file, with the original path already available as `request.path`.
That is a front controller with no new concepts.

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
