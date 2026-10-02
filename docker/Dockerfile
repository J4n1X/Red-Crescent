# syntax=docker/dockerfile:1
# Build from the repository root:
#   docker build -f docker/Dockerfile -t red-crescent .

FROM rust:1-bookworm AS build

# `lua54` or `luajit`. Extra features (e.g. `c-modules`) go in FEATURES.
ARG LUA=lua54
ARG FEATURES=""

WORKDIR /src
COPY . .
# target/ is a cache mount, so the binary is copied out in the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --no-default-features \
        --features "$LUA${FEATURES:+,$FEATURES}" \
 && install -m755 target/release/Red-Crescent /usr/local/bin/Red-Crescent

FROM debian:bookworm-slim

ARG UID=10001
ARG GID=$UID
RUN groupadd --system --gid "$GID" rc \
 && useradd --system --uid "$UID" --gid rc --home-dir /data --shell /usr/sbin/nologin rc \
 && mkdir -p /app /data \
 && chown rc:rc /data

COPY --from=build /usr/local/bin/Red-Crescent /usr/local/bin/Red-Crescent
# Something to look at until a real app is mounted over /app.
COPY demos /app

# Env beats rc_config.lua, so an app's own `bind` and `data_dir` cannot point
# the server somewhere the container does not publish or persist.
ENV RC_BIND=0.0.0.0:8080 \
    RC_SERVE_DIR=/app \
    RC_DATA_DIR=/data \
    RUST_LOG=info

USER rc
WORKDIR /data
VOLUME /data
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/Red-Crescent"]
