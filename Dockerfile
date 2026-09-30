# syntax=docker/dockerfile:1

# Build once, ship small. The runtime stage is debian-slim plus the six packages
# the action scripts need, so there is no compiler, no cargo registry and no
# source in the published image.

ARG RUST_VERSION=1.98

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src

# Dependencies first, against a stub binary, so the ~30 dependency crates stay
# in a cached layer and only meshbot-rs recompiles when src/ changes.
#
# --locked is load-bearing: it makes a stale or hand-edited Cargo.lock a build
# failure instead of a silent in-place rewrite. It is also why the release
# pipeline has to keep Cargo.lock's `meshbot-rs` version in step with
# Cargo.toml, which release-please's `rust` release type does on its own.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && printf 'fn main() {}\n' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src

# The real tree. meshbot.pest comes along because parse.rs pulls it in through
# `#[grammar = "meshbot.pest"]`, which the stub above never needed.
COPY src ./src

# The touch is not optional. Cargo fingerprints by mtime, and a COPY preserves
# the mtimes the files have on the host — which for a long-lived checkout are
# OLDER than the artifacts the stub build just produced. Without this, cargo
# concludes the crate is already fresh and the release build silently keeps the
# stub `fn main() {}`: an image that starts, exits 0, and does nothing.
RUN find src -type f -exec touch {} + \
 && cargo build --release --locked

FROM debian:bookworm-slim

# Action scripts are execve'd with an emptied environment and no PATH: the
# executor calls env_clear() and then adds only the names a verb entry declares
# (src/script.rs). So every binary a script may invoke has to exist here at an
# absolute path, and adding one here is the only way a script can reach it.
#
#   bash             for templates that want it rather than #!/bin/sh
#   ca-certificates  curl fails TLS to HA/Komodo/PVE without a trust store
#   curl             ha-entity.sh, ha-service.sh, komodo-status.sh, pve-reboot.sh
#   jq               the same four, for parsing their JSON
#   iputils-ping     internet-status.sh, which degrades to a TCP check if absent
#   netbase          /etc/protocols and /etc/services, which iputils-ping drops
#
# Keep this list and scripts.example/README.md in step: a script naming a tool
# that is not installed here fails at message time, not at build time.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash \
      ca-certificates \
      curl \
      jq \
      iputils-ping \
      netbase \
 && rm -rf /var/lib/apt/lists/*

# The script allowlist directory is deliberately NOT created. script_dir()
# canonicalizes it at startup and treats absence as a hard error, so a missing
# compose mount stops the bot loudly instead of leaving it answering every
# command with "action failed" while looking healthy. Creating the directory
# here would throw that away.
#
# This WORKDIR exists for a different reason: dotenvy searches upward from the
# current directory for `.env`, and with CWD=/ it would never look in /data at
# all. main.rs ignores the dotenv error, so a wrong WORKDIR does not fail
# startup — it just leaves every credential missing until an action runs.
WORKDIR /data/meshcore/meshbot-rs

COPY --from=build /src/target/release/meshbot-rs /usr/local/bin/meshbot-rs

# Numeric rather than a named user: nothing here calls getpwuid(), so there is
# no reason to depend on the `passwd` package being present in -slim. It also
# states the uid the host must be able to read, which is the part that matters:
# /data/meshcore/meshbot-rs and everything mounted under it have to be world
# readable and traversable, or this user silently cannot read the .env.
USER 1000:1000

# The source label is what links the published GHCR package to this repository,
# which is what lets the workflow keep pushing to it on later runs.
LABEL org.opencontainers.image.source="https://github.com/mathieuruellan/meshbot-rs"
LABEL org.opencontainers.image.title="meshbot-rs"
LABEL org.opencontainers.image.description="MeshCore channel rule/action bot"

ENTRYPOINT ["/usr/local/bin/meshbot-rs"]
