# Relix container image.
#
# Multi-stage build:
#   * Stage 1 (`builder`) — full Rust toolchain on Debian bookworm,
#     pinned to a content-addressed digest, compiles
#     relix-cli + relix-controller + relix-web-bridge in release mode
#     with all the system development headers that rustls / libp2p /
#     rusqlite (bundled) / prost need at build time.
#   * Stage 2 (`runtime`) — Google Distroless `cc-debian12:nonroot`,
#     pinned to a content-addressed digest, ships only glibc +
#     libstdc++ + libgcc + libssl + ca-certificates + tzdata + the
#     three Relix binaries. No shell, no package manager, no curl, no
#     build tools. The image runs as the distroless `nonroot` user
#     (UID 65532) by default — there is no path for a compromised
#     binary to fork a shell or apt-get anything inside the container.
#
# Per the supply-chain policy (PART 6 — gap-report.md):
#   * Both base images are pinned to immutable sha256 digests so a
#     mutable upstream tag re-publish cannot silently substitute the
#     content. Update the digest deliberately when bumping bases.
#   * Stage 2 carries zero build tools — no gcc, no make, no apt, no
#     dev headers, no compilers, no curl, no shell.
#   * Stage 2 runs as a non-root user (built-in `nonroot`, UID 65532)
#     and never executes as root.
#   * Only the three release binaries are COPYed from `builder`. No
#     source, no tests, no cargo cache.
#
# Build:
#   docker build -t relix .
#
# Run (bridge against an external mesh):
#   docker run --rm -p 19791:19791 \
#     -v $PWD/dev-data:/relix/dev-data \
#     -v $PWD/dev-keys:/relix/dev-keys \
#     relix \
#     --config /relix/configs/bridge.toml

# ─── builder ───────────────────────────────────────────────
# rust:1.95.0-bookworm — multi-arch index digest from Docker Hub.
# To refresh: `docker buildx imagetools inspect rust:1.95.0-bookworm`
# and copy the top-level digest.
FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS builder

# Build-time system deps for the workspace:
#   - pkg-config + libssl-dev: rustls-tls is the default, but some
#     dev-only features (test fixtures, libp2p variants) still touch
#     OpenSSL at link time.
#   - libsqlite3-dev: kept for parity even though rusqlite is built
#     with `bundled`. Keeps the build resilient if a transitive crate
#     opts into the system sqlite.
#   - protobuf-compiler: libp2p-noise → prost-build needs `protoc`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev libsqlite3-dev protobuf-compiler ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /relix
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Compile only the three operator-facing binaries — dev / inspect /
# test crates stay out of the image to keep build time tight and the
# final image footprint at three binaries.
RUN cargo build --release \
        -p relix-controller \
        -p relix-web-bridge \
        -p relix-cli

# ─── runtime ───────────────────────────────────────────────
# gcr.io/distroless/cc-debian12:nonroot — linux/amd64 digest pulled
# from gcr.io. The :nonroot tag pre-creates a `nonroot` user (UID
# 65532, GID 65532) and sets `USER nonroot` as the default; no
# explicit useradd / USER directive is needed in this stage.
#
# To refresh: pull the tag and read the digest with
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian12:nonroot
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:7f376818a75cc40e78ea271a2f96102377059e5e860cf4b0253d886b5e3a9034 AS runtime

# Only the three release binaries cross the stage boundary. No source,
# no cargo cache, no headers, no compilers.
COPY --from=builder /relix/target/release/relix-controller /usr/local/bin/relix-controller
COPY --from=builder /relix/target/release/relix-web-bridge /usr/local/bin/relix-web-bridge
COPY --from=builder /relix/target/release/relix-cli        /usr/local/bin/relix-cli

# Default mountpoint for keys + per-run data. Operators bind-mount
# their own dev-keys/ + dev-data/ + configs/ over these so state
# survives container restart. The base image's `nonroot` user
# already owns /home/nonroot; /relix is created here for the bind
# mounts and must be writable by the runtime user.
WORKDIR /relix

# Bridge HTTP port (loopback in dev, exposed in compose).
EXPOSE 19791

# Distroless ships no shell and no curl, so the previous shell-form
# HEALTHCHECK is intentionally removed. Orchestrators (Docker Compose
# `healthcheck`, Kubernetes `livenessProbe.httpGet`, ECS task health
# checks) MUST probe GET http://<host>:19791/health directly from the
# orchestrator side — that's also the more reliable place for the
# probe because it tests the network path the actual traffic uses.
# See docs/security.md for the /health route shape.

# Default to the bridge so `docker run relix` Just Works once a
# bridge.toml is mounted in. Override the ENTRYPOINT array (or run a
# different binary explicitly: `docker run relix relix-controller`)
# when running a controller node.
ENTRYPOINT ["/usr/local/bin/relix-web-bridge"]
CMD ["--config", "/relix/configs/bridge.toml"]
