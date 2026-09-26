# openpasture server: API, collar endpoints, live feed and web UI in one binary.
#
#   docker build -t openpasture .
#   docker run -d --name openpasture -p 7878:7878 -v openpasture:/data openpasture
#   docker exec openpasture openpasture token

FROM oven/bun:1 AS ui
WORKDIR /src/ui
COPY ui/package.json ui/bun.lock ./
RUN bun install --frozen-lockfile
COPY ui/ ./
RUN bun run build

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
COPY --from=ui /src/ui/dist ui/dist
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=openpasture-target,target=/src/target \
    cargo build --release --locked -p op-cli \
    && cp target/release/openpasture /usr/local/bin/openpasture

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /data --shell /usr/sbin/nologin openpasture \
    && mkdir -p /data \
    && chown openpasture:openpasture /data
COPY --from=build /usr/local/bin/openpasture /usr/local/bin/openpasture
ENV OPENPASTURE_DATA_DIR=/data
VOLUME /data
EXPOSE 7878
USER openpasture
CMD ["openpasture", "serve", "--bind", "0.0.0.0", "--port", "7878"]
