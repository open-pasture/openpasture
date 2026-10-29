# openpasture server: API, collar endpoints, live feed and web UI in one binary.
#
#   docker build -t openpasture .
#   docker run -d --name openpasture -p 127.0.0.1:7878:7878 -v openpasture:/data openpasture
#   docker exec openpasture openpasture token
#
# Publish on 127.0.0.1 and put a TLS proxy in front (docs/SELF-HOSTING.md);
# `-p 7878:7878` serves plain HTTP on every interface. tini runs as PID 1 so
# `docker stop` reaches the server as SIGTERM (it drains and closes the
# database) and the CLI brains' child processes are reaped.

# Bases are pinned by digest (Dependabot keeps them current); the Rust
# version matches rust-toolchain.toml.
FROM oven/bun:1.4.0@sha256:5ff609364c049b54eb0ff560ec96319729a972078ef2c755d758f0c6ef89c2d6 AS ui
WORKDIR /src/ui
COPY ui/package.json ui/bun.lock ./
RUN bun install --frozen-lockfile
COPY ui/ ./
RUN bun run build

FROM rust:1.99.0-slim-bookworm@sha256:2c3a22f0a5533ea2dd5a16627bc841228151faa2d4de2644ac9987e4a2f1f2fa AS build
WORKDIR /src
COPY . .
COPY --from=ui /src/ui/dist ui/dist
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=openpasture-target,target=/src/target \
    cargo build --release --locked -p op-cli \
    && cp target/release/openpasture /usr/local/bin/openpasture

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /data --shell /usr/sbin/nologin openpasture \
    && mkdir -p /data \
    && chown openpasture:openpasture /data
COPY --from=build /usr/local/bin/openpasture /usr/local/bin/openpasture
# THIRD-PARTY-NOTICES is written by scripts/third-party-notices.py; release
# builds always have it, a local build without it still works.
COPY LICENSE NOTICE THIRD-PARTY-NOTICES* /usr/share/doc/openpasture/
ENV OPENPASTURE_DATA_DIR=/data
VOLUME /data
EXPOSE 7878
USER openpasture
# Asks the server named in /data/server.json for /healthz: unhealthy when it
# doesn't answer, its database doesn't, or a job that drives fences or sends
# alerts has been failing for 5 minutes ("down", 503). A job restarting for
# less is "degraded", still healthy; see /api/server for which. While the
# database is upgrading ("migrating") it stays healthy: killing it half-way
# would only start the upgrade again.
HEALTHCHECK --interval=30s --timeout=10s --start-period=120s --retries=3 CMD ["openpasture", "health"]
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["openpasture", "serve", "--bind", "0.0.0.0", "--port", "7878"]
