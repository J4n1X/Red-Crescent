# Running Red Crescent in Docker

Build from the repository root:

```bash
docker build -f docker/Dockerfile -t red-crescent .
docker run --rm -p 8080:8080 red-crescent          # serves the demos
```

Mount your own app over `/app` and give `/data` a volume:

```bash
docker run -d -p 8080:8080 \
  -v "$PWD/my-app:/app:ro" \
  -v my-app-data:/data \
  red-crescent
```

`compose.yaml` does the same for Drive (`up --build drive`) and for a plugin example
(`up --build hostname`, see below).

| Path    | What                                                  |
|---------|-------------------------------------------------------|
| `/app`  | serve directory, `rc_config.lua` is read from here    |
| `/data` | data directory: databases, uploads, spool — persist it |
| `/opt/red-crescent/plugins` | plugins built into the image, root-owned |

The image sets `RC_BIND`, `RC_SERVE_DIR` and `RC_DATA_DIR`, which override the same keys in
`rc_config.lua`. Any other setting can be passed as an `RC_*` variable or a flag after the image
name, e.g. `docker run red-crescent --dev`.

The server runs as uid/gid 10001, changeable with `--build-arg UID=… --build-arg GID=…` (GID
defaults to UID). A named volume works as is; a host directory mounted on `/data` must be
writable by that uid.

Build arguments:

```bash
docker build -f docker/Dockerfile --build-arg LUA=luajit -t red-crescent:luajit .
docker build -f docker/Dockerfile --build-arg FEATURES=c-modules -t red-crescent:cmod .
```

## Plugins

`PLUGINS` names workspace plugin packages to build into the image:

```bash
docker build -f docker/Dockerfile --build-arg PLUGINS=hostname -t red-crescent:hostname .
```

Each lands in `/opt/red-crescent/plugins` and is enabled from the app's `rc_config.lua`:

```lua
plugins = { hostname = { path = "/opt/red-crescent/plugins/hostname" } }
```

`examples/hostname/` is such an app. A plugin built elsewhere goes in with a derived image,
built against the same Debian release:

```dockerfile
FROM red-crescent
COPY libmyplugin.so /opt/red-crescent/plugins/
```

A plugin is native code in the server's process, so the container is its sandbox. The
`compose.yaml` services run with a read-only root filesystem, no capabilities and
`no-new-privileges`; keep that for anything running third-party plugins. What it does not
stop: a plugin can still read the app, read and change everything in `/data`, and reach the
network. The container protects the host from the app, not the app from its plugins.

As on bare metal, put a TLS-terminating proxy in front of it.
