# Red Crescent

A web server that renders HTML with embedded Lua, written in Rust. Think PHP, but with Lua. It is
meant to run behind nginx, Apache or Caddy, though it serves static files itself too.

- **Lua templates**: `<?lua ... ?>` blocks and `<?lua= expr ?>` expressions, HTML-escaped by
  default, with loops and conditionals spanning blocks.
- **Batteries included**: request and response APIs, multipart uploads, SQLite, argon2 passwords,
  subprocesses without a shell, and background threads.
- **Guard rails**: a per-request timeout and memory limit, and each request isolated in its own
  environment.

[apps/drive](apps/drive/) is a complete multi-user file-sharing app written in Lua on these APIs:
`cargo run --release -- --serve-dir apps/drive`.

The full reference for templates, APIs, configuration and the security model is in
**[API.md](API.md)**.

## Getting started

Building needs Rust and a C compiler; Lua 5.4 is compiled in.

```bash
git clone https://github.com/J4n1X/Red-Crescent.git
cd Red-Crescent
cargo run --release -- --dev
```

Then open `http://127.0.0.1:8080/`, which serves the demos in `demos/`. `--dev` shows detailed
errors with `.lhtml` file and line numbers, and picks up template edits without a restart.

### Your first template

Every `.lhtml` file in the serve directory is a page:

```html
<!-- hello.lhtml, served at /hello.lhtml -->
<h1>Hello, <?lua= request.query.name or "World" ?>!</h1>
<ul>
    <?lua for _, fruit in ipairs({"Apples", "Bananas"}) do ?>
        <li><?lua= fruit ?></li>
    <?lua end ?>
</ul>
```

`<?lua= ?>` escapes its value; `<?lua== ?>` and `print()` do not, so user input must never reach
them unescaped. Other files are served as static assets, except `.lua` modules and dotfiles.

### Configuration

Serve your own directory with `--serve-dir`. Settings come from flags, then `RC_*` environment
variables, then an optional `rc_config.lua` in the serve directory, so an app can ship its own:

```lua
-- rc_config.lua
return {
    data_dir = "./data",                  -- SQLite databases and uploads; outside the serve dir
    max_upload_size = 1024 * 1024 * 1024,
    timeout_ms = 10000,
}
```

`--help` lists every option; [API.md](API.md#configuration) explains them.

## Deploying on Linux with systemd

Red Crescent speaks plain HTTP. **Put it behind a reverse proxy with TLS**, or logins and session
cookies cross the network in cleartext.

The recommended setup runs one process per app. systemd owns the socket and starts the app on the
first request; nginx proxies to the socket. Each app lives in its own directory:

```
/var/www/<app>/Red-Crescent   the binary, deployed together with the app
/var/www/<app>/app/           serve directory
/var/www/<app>/data/          data directory, writable by user <app>
/etc/red-crescent/<app>.env   optional RC_* overrides
```

The binary sits beside each app so apps upgrade independently: templates are written for the
binary they shipped with.

**1. Install the app** (here called `blog`) under its own system user:

```bash
cargo build --release
sudo useradd --system --home-dir /var/www/blog --shell /usr/sbin/nologin blog
sudo mkdir -p /var/www/blog/data
sudo cp -r path/to/your/app /var/www/blog/app
sudo install -m755 target/release/Red-Crescent /var/www/blog/
sudo chown -R blog:blog /var/www/blog
```

**2. Enable the socket** with the template units from `contrib/systemd/`:

```bash
sudo cp contrib/systemd/red-crescent@.* /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now red-crescent@blog.socket
```

This creates `/run/red-crescent/blog.sock`, which only nginx (`www-data`) may connect to. If the
app starts background threads from `rc_config.lua` that must run before anyone visits, also
enable `red-crescent@blog.service`.

**3. Proxy to it from nginx:**

```nginx
upstream blog {
    server unix:/run/red-crescent/blog.sock;
    keepalive 16;
    keepalive_timeout 4s;   # below the app's 5s, so nginx never reuses a closing connection
}

server {
    server_name blog.example.com;
    # listen 443 ssl and certificates, e.g. from certbot

    client_max_body_size 1g;   # at least the app's max_upload_size
    proxy_http_version 1.1;
    proxy_set_header Connection "";
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;

    location / { proxy_pass http://blog; }
}
```

Over a Unix socket `request.remote_addr` is nil; apps read the visitor's address from
`X-Real-IP`.

**To update**, stop both units, replace the binary and app together, and start them again:

```bash
sudo systemctl stop red-crescent@blog.socket red-crescent@blog.service
# replace /var/www/blog/Red-Crescent and /var/www/blog/app
sudo systemctl start red-crescent@blog.socket red-crescent@blog.service
```

Stopping the socket matters: while it listens, a request would start the app halfway through the
swap.

The server also runs without systemd: it binds `--bind` (default `127.0.0.1:8080`) itself and can
be proxied to over TCP the same way.

## Security at a glance

Templates are trusted code, like PHP files: they can run programs and read files as the server
user. The server protects against runaway templates, path traversal, source disclosure and error
leakage, and isolates requests from each other. Details and limits are under
[Security model](API.md#security-model).

## Development

```bash
cargo test          # unit and integration tests
cargo clippy --all-targets
cargo run -- --dev  # local server with detailed errors and no caching
```

## License

MIT. See [LICENSE](LICENSE).
