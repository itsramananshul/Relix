# Running Relix in Docker

W2-008g — operator walkthrough for containerized deploys.

## Docker and Qdrant in the setup wizard

`relix setup` checks Docker availability at startup via
`docker info`. If Docker is not running when you choose
**"with memory"** (Qdrant) during setup, the wizard exits
with an actionable message — start Docker and re-run
`relix setup`.

Qdrant is installed by `relix install --fix` using:

```bash
docker run -d --name relix-qdrant \
    -p 6333:6333 -p 6334:6334 qdrant/qdrant
```

The **"without memory"** option (the default) skips Qdrant
entirely and runs without vector memory. You can add memory
later by re-running `relix setup` and choosing option `[1]`.

The repo ships a multi-stage `Dockerfile` at the workspace
root that builds `relix-controller`, `relix-web-bridge`,
and `relix-cli` and packages them into a slim Debian
runtime image. The bridge is the default entrypoint; the
controller binary is baked in so the same image can run
memory / ai / coord nodes off different argv.

## Build

```bash
docker build -t relix .
```

First build is slow (compiles the workspace in release
mode). Subsequent builds reuse the layer cache when only
config / docs change.

## Run the bridge against an existing local mesh

The simplest scenario: you already started the memory + ai
controllers on the host (`./scripts/relix-mesh-up.sh --keep`
or similar) and want to expose the bridge HTTP from a
container. Use host networking so the bridge can dial
the loopback peers:

```bash
docker run --rm \
    --network=host \
    -v "$PWD/dev-keys:/relix/dev-keys:ro" \
    -v "$PWD/dev-data/local:/relix/dev-data:ro" \
    -v "$PWD/configs:/relix/configs:ro" \
    -v "$PWD/flows:/relix/flows:ro" \
    relix \
    /usr/local/bin/relix-web-bridge --config /relix/dev-data/bridge.toml
```

`--network=host` makes `127.0.0.1` inside the container
resolve to the host's loopback, so the bridge can dial the
memory / ai controllers via the existing `peers.toml`
without rewriting any addresses. (Linux-native Docker
only; Docker Desktop's host-network mode has caveats on
Mac / Windows.)

The volumes are mounted read-only because the bridge
doesn't write back to keys / configs; only `dev-data/`
holds the SQLite memory db (which lives on the memory
controller, not the bridge — so even that can be RO from
the bridge's perspective).

## Run a controller in a container

To run, say, the memory controller from inside docker:

```bash
docker run --rm \
    --network=host \
    -v "$PWD/dev-keys:/relix/dev-keys" \
    -v "$PWD/dev-data/local:/relix/dev-data" \
    -v "$PWD/configs:/relix/configs:ro" \
    relix \
    /usr/local/bin/relix-controller --config /relix/dev-data/memory.toml
```

Notice `dev-data/` is RW now because the memory
controller persists the SQLite db there.

## Smoke from inside docker

The `relix-cli` binary is baked into the image, so
`relix-cli ops smoke` works against any reachable bridge:

```bash
docker run --rm --network=host relix \
    /usr/local/bin/relix-cli ops smoke
```

CI-friendly: the smoke returns exit 1 on any failed step.

## Image layout

| Path                              | Purpose                                |
| --------------------------------- | -------------------------------------- |
| `/usr/local/bin/relix-controller` | mesh controller binary                 |
| `/usr/local/bin/relix-web-bridge` | HTTP bridge binary                     |
| `/usr/local/bin/relix-cli`        | operator CLI                           |
| `/relix/dev-data`                 | mount your `dev-data/<run>` directory  |
| `/relix/dev-keys`                 | mount your `dev-keys/` directory       |
| `/relix/configs`                  | mount your `configs/` directory        |

The runtime image runs as UID 1000 (`relix:relix`) so a
Linux host bind-mount of `dev-data/` retains correct
ownership.

## Out of scope

- **Multi-container compose**: no `docker-compose.yml` ships
  with the repo today. The mesh's expectation of a
  pre-minted identity tree (`relix-cli identity init-org`
  → `relix-cli identity mint` for every node) doesn't
  compose-orchestrate cleanly without an init container,
  which would dilute the "honesty contract" by hiding the
  ceremony. Operators wanting full container deploys should
  port `scripts/relix-mesh-up.sh` into their own compose /
  k8s manifests using this image as the base.
- **Production-grade hardening**: the image runs as a
  non-root user, but doesn't include seccomp / AppArmor
  profiles. The bridge enforces bearer-token auth on all
  non-public routes, but for internet-facing deployments you
  still need a reverse proxy with TLS + external auth in front
  (see `docs/deployment.md` and `docs/bridge-invariants.md`).
  The bridge bearer token is stored at `~/.relix/bridge-token`
  inside the container; mount a persistent volume or read it
  from the container logs on first start to use it from
  external clients.
