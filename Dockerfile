# W2-008g — Relix container image.
#
# Multi-stage build that compiles the controller +
# web-bridge in a Rust toolchain stage and copies just the
# binaries into a slim Debian runtime. Total size is on the
# order of ~100MB (Debian slim + libssl + sqlite) — not
# tiny, but small enough for ops use and trivially reachable
# from `docker compose up`.
#
# The bridge is the default entrypoint; the controller
# binary is also baked in so `docker compose` can run
# memory / ai / coord nodes off the same image with
# different argv.
#
# Build:
#   docker build -t relix .
#
# Run (bridge against an external mesh):
#   docker run --rm -p 19791:19791 \
#     -v $PWD/dev-data:/relix/dev-data \
#     -v $PWD/dev-keys:/relix/dev-keys \
#     relix \
#     /usr/local/bin/relix-web-bridge --config /relix/dev-data/local/bridge.toml

# ─── builder ───────────────────────────────────────────────
FROM rust:1.95.0-bookworm AS builder

# System deps the workspace needs to link:
#   - pkg-config + libssl-dev: reqwest TLS
#   - libsqlite3-dev:           rusqlite bundled-with-system mode
#   - protobuf-compiler:        libp2p-noise via prost (when feature-enabled)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev libsqlite3-dev protobuf-compiler ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /relix
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Compile the two operator-facing binaries in release mode.
# We deliberately skip --workspace so dev / inspect / test
# crates don't bloat the image footprint.
RUN cargo build --release \
        -p relix-controller \
        -p relix-web-bridge \
        -p relix-cli

# ─── runtime ───────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# libssl3 + libsqlite3-0 + ca-certificates are needed at
# runtime (we link dynamically). Tini gives us a proper
# init so ^C / SIGTERM forwards cleanly to the bridge.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libssl3 libsqlite3-0 ca-certificates tini curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root user — the bridge has no business running as
# root. UID 1000 matches the conventional first non-root
# uid so bind-mounted dev-data/ from a Linux host shares
# ownership cleanly.
RUN groupadd --gid 1000 relix \
    && useradd  --uid 1000 --gid 1000 --create-home --shell /usr/sbin/nologin relix

COPY --from=builder /relix/target/release/relix-controller /usr/local/bin/relix-controller
COPY --from=builder /relix/target/release/relix-web-bridge /usr/local/bin/relix-web-bridge
COPY --from=builder /relix/target/release/relix-cli        /usr/local/bin/relix-cli

# Default mountpoint for keys + per-run data. Operators
# bind-mount their own dev-keys/ + dev-data/ over these so
# state survives container restart.
RUN mkdir -p /relix/dev-data /relix/dev-keys /relix/configs \
    && chown -R relix:relix /relix
USER relix
WORKDIR /relix

# Bridge HTTP port (loopback in dev, exposed in compose).
EXPOSE 19791

# Container-level liveness probe — orchestrators (docker compose,
# Kubernetes via `livenessProbe.exec`, ECS) restart the container
# when this exits non-zero three times in a row. /health is the
# plaintext liveness route the bridge exposes specifically for
# probes; it requires no auth (see docs/security.md).
HEALTHCHECK --interval=30s --timeout=10s --retries=3 \
  CMD curl -f http://localhost:19791/health || exit 1

# Default to the bridge so `docker run relix` Just Works
# once a bridge.toml is mounted in. Override the ENTRYPOINT
# array (or pass `--config` plus a different binary) when
# running a controller node.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/relix-web-bridge", "--config", "/relix/configs/bridge.toml"]
