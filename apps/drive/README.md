# Drive

A multi-user file browser built **entirely in Lua** on top of Red Crescent — uploads, downloads,
virtual folders, public share links, sessions, and an SQLite database. No Rust code in here:
everything under this directory is `.lhtml` pages and `.lua` modules using the platform's
built-in `sqlite`, `crypto`, `request.files`, `response.send_file` and background-job APIs.

## Run it

```bash
cargo run --release -- --serve-dir apps/drive
```

No other flags needed: `rc_config.lua` in this directory declares the server settings Drive
requires (8 GiB / 20000-file upload limits, a 30s execution budget for archiving, the data
directory, and the cleanup thread). Flags and `RC_*` environment variables still override it.

Two config files, deliberately separate: **`rc_config.lua`** configures the *server*
(limits, binding, threads) and is read by Red Crescent; **`config.lua`** configures the
*app* (share-link base, session lifetime, archive ceilings) and is read by Drive's own Lua.

Open `http://127.0.0.1:8080/` and register — **the first account becomes the admin**.
Every later registration is held as "pending" until an admin approves it on the Admin page.

The housekeeping thread spawns itself: every request calls
`thread.spawn("drive-cleanup", "jobs/cleanup.lua")`, which is an idempotent no-op while the
thread is alive and resurrects it if it ever died. Add `--thread jobs/cleanup.lua` if you
want it running from boot rather than from the first request.

## What's inside

| Path | Role |
|---|---|
| `index.lhtml` | Folder view: breadcrumbs, listing, file & folder upload, new-folder, per-row actions |
| `actions.lhtml` | POST target for all mutations (CSRF-checked), redirects back with a flash |
| `manage.lhtml` | Rename, move and delete one file **or folder** — the no-JavaScript path for the listing's Manage icon |
| `download.lhtml` | Authenticated download of a single file via `response.send_file` |
| `archive.lhtml` | Folder downloads: queue a job, watch it, collect the finished archive |
| `shares.lhtml` | All your share links, or create/revoke the ones for a single item (`?file=` / `?folder=`) |
| `app.lhtml` | Front controller for paths that are not files: routes `/s/<token>`, 404s the rest |
| `s.lhtml` | **Public** share page: a single file, or a browsable shared folder, reached as `/s/<token>` |
| `login/register/logout/admin.lhtml` | Auth flow and user management |
| `config.lua` | App settings: share-link base URL, session lifetime, archive ceilings |
| `rc_config.lua` | Server settings Red Crescent reads at startup |
| `lib/*.lua` | `db` (schema + migrations), `auth` (sessions/CSRF), `files`, `shares`, `archive`, `util` |
| `jobs/cleanup.lua` | Self-looping background thread: GC for expired sessions/shares, orphaned files, stale spool |
| `jobs/archive.lua` | One-shot background task: builds a single queued folder archive, then exits |

Data lives in the platform data directory: `drive/drive.db` (SQLite, WAL) and
`drive/files/<random>` for file bytes — user-supplied names never touch the disk.

**Folder uploads**: the second upload form uses `webkitdirectory`; the browser submits every
file with its relative path, and Drive recreates the folder tree as virtual folders
(merging into existing ones). Browsers that submit plain filenames instead of relative
paths degrade gracefully — the files land flat in the current folder.

**Scale**: measured on a real 1.2 GiB / 4487-file / 211-folder game directory. The upload
(transfer + storing the whole tree) takes about 1.5s, and archiving all of it into a 950 MiB
zip takes about 33s; server memory stays around 15–40 MiB throughout, since both directions
stream. Two things make the upload fast: the whole batch is stored in **one SQLite
transaction** (per-file commits meant an fsync each, which used to blow the execution limit
and leave a half-stored tree), and folder lookups are cached per batch instead of repeated
per file. Archives are **not** built in the request: `archive.lhtml` inserts a row in `archive_jobs` and
spawns a thread named after it, so a multi-gigabyte export no longer holds an upstream connection
open or needs a request timeout sized for it. Each export is a **disposable task** — one job,
then the thread exits — rather than a service that idles between them, so two people exporting
different folders do not queue behind each other. `archive_max_concurrent` bounds how many run
at once.

The row survives the thread on purpose: it is what the polling page reads, and it outlives a
restart where the thread registry does not. Naming the thread after the job makes spawning
idempotent per export, so a double-click cannot start a second build. Every poll calls
`ensure_running`, which is where the self-healing lives: a row marked `building` with no live
thread behind it was lost to a restart or a dead task, and is put back in line.

**Folder downloads** (`lib/archive.lua`) run whatever archiver the host has, discovered at
runtime: `zip` if present, otherwise `tar` (`.tar.gz`), and if neither exists the feature
simply isn't offered — no download links appear and a hand-typed archive URL returns a
friendly message. Compression runs at level 1 on purpose: on the test data it produced
950 MiB in 24s versus 945 MiB in 30s at the default level. Because file bytes live under random names, an archive is built by
staging a tree of **hard links** (real names and structure, no data copied) and running the
archiver inside it, so no user-controlled name ever reaches the command line; names that do
reach the shell (as link targets) are single-quoted and passed after `--`. Exports are
capped at 2000 files / 512 MiB, bounded by `timeout(1)` when available, and the finished
archives land in `drive/tmp/` where the cleanup thread sweeps them — along with their
`archive_jobs` rows, so a `ready` row can never outlive the file it points at.

## Security model

- Passwords are argon2id hashes (`crypto.password_hash`).
- Sessions: 32-byte random token in an `HttpOnly` `SameSite=Lax` cookie, stored **hashed**
  (sha256) in the DB, 7-day expiry enforced on every request.
- Every mutation goes through `actions.lhtml` with a per-session CSRF token compared in
  constant time.
- Every file/folder/share query filters by `owner_id` in SQL — ids from other users 404.
- **Share link addresses**: by default a link is built from the request, preferring
  `X-Forwarded-Proto` / `X-Forwarded-Host` (so it comes out right behind an HTTPS reverse
  proxy) and falling back to `Host`. That means the link carries whatever address *you*
  used — reach the app over a LAN or Tailscale IP and the link will contain that IP, which
  the person you send it to cannot resolve. Set `share_base_url` in `config.lua` to pin a
  public address once the app has one; it then wins over anything in the request.
- Share links are `/s/<token>`, an unguessable 128-bit token; expiry is checked at access time
  (the cleanup job only garbage-collects). A **folder share** lets anyone with the link
  browse that folder, descend into it, download single files, and grab any subtree as an
  archive — but the share root is a hard boundary: every folder and file id is verified to
  be inside it (`files.is_within`), so guessing a neighbouring id returns "not part of this
  share" rather than someone else's data.
- All user-controlled strings are `html_escape`d before rendering.

Known limitations (deliberate, documented): no login rate limiting; the session cookie's
`Secure` flag is left to your HTTPS reverse proxy; no per-user storage quotas.
