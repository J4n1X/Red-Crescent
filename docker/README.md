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

`compose.yaml` does the same for Drive: `docker compose -f docker/compose.yaml up --build`.

| Path    | What                                                  |
|---------|-------------------------------------------------------|
| `/app`  | serve directory, `rc_config.lua` is read from here    |
| `/data` | data directory: databases, uploads, spool — persist it |

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

As on bare metal, put a TLS-terminating proxy in front of it.
