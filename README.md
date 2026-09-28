# openpasture

Runs virtual fence collars, stores their data, analyses it, and decides where the herd goes.
One Rust server with the web UI built in. The desktop app runs that server for you.

- Collars report positions over HTTP; boundaries go back to them signed, and each collar acks.
- Draw paddocks and boundaries on a satellite map and watch the herd live.
- Collar and fence health, tracks and replay, heatmaps, grazing pressure and rest, an SQL
  console, and CSV, GeoJSON and Parquet export.
- A daily grazing decision from Codex or Claude Code (your own sign-in), an API key, or a
  built-in heuristic. You approve moves, or let them apply on a timer or at once.
- Strips on a schedule, staged on the collars so they open when the server or signal is away.
- Alerts, approvals and a morning brief by text with your own Twilio number, or by email, a
  webhook or push. People and roles: owner, manager, hand, viewer; hands who only text need no
  sign-in.
- Grazing records, NRCS 528, organic season, lease head-days and a welfare record, in acres or
  hectares.
- An MCP server, so Claude, ChatGPT or any agent can run the farm with the same tools.

## Download

| | |
| --- | --- |
| macOS app (Apple Silicon and Intel) | [openpasture-macos.dmg](https://github.com/open-pasture/openpasture/releases/latest/download/openpasture-macos.dmg) |
| Server, Linux x64 | [openpasture-server-linux-x64.tar.gz](https://github.com/open-pasture/openpasture/releases/latest/download/openpasture-server-linux-x64.tar.gz) |
| Server, Linux arm64 (e.g. Raspberry Pi 4 or 5, 64-bit OS) | [openpasture-server-linux-arm64.tar.gz](https://github.com/open-pasture/openpasture/releases/latest/download/openpasture-server-linux-arm64.tar.gz) |
| Server, macOS Apple Silicon | [openpasture-server-macos-arm64.tar.gz](https://github.com/open-pasture/openpasture/releases/latest/download/openpasture-server-macos-arm64.tar.gz) |
| Server, macOS Intel | [openpasture-server-macos-x64.tar.gz](https://github.com/open-pasture/openpasture/releases/latest/download/openpasture-server-macos-x64.tar.gz) |
| Docker | `ghcr.io/open-pasture/openpasture` |

Checksums are in `SHA256SUMS` on each [release](https://github.com/open-pasture/openpasture/releases).

## Run the server

```
mkdir openpasture && cd openpasture
tar xzf ../openpasture-server-linux-x64.tar.gz
./openpasture serve --bind 0.0.0.0 --port 7878
```

Open `http://<host>:7878`. On the machine itself (`127.0.0.1`) no token is needed. From anywhere
else the app asks for its token once:

```
./openpasture token
```

Data lives in `OPENPASTURE_DATA_DIR`, else the platform data directory; `--data-dir DIR`
overrides both. It is one SQLite database plus Parquet files, so a copy of the directory is a
backup.

### Docker

```
docker run -d --name openpasture -p 7878:7878 -v openpasture:/data ghcr.io/open-pasture/openpasture
docker exec openpasture openpasture token
```

The image runs as a non-root user and keeps everything in the `/data` volume.

For a farm: an https address for collars and phones, systemd, backups and upgrades are in
`docs/SELF-HOSTING.md`; texting and alerts in `docs/TEXTING.md`.

## Try it with simulated collars

Until you have a collar, `collar-sim` (in the server tarball, or `cargo run -p collar-sim`)
speaks the real collar protocol. In the app, name the farm, draw a paddock and create a herd.
Then:

```
./collar-sim --server http://127.0.0.1:7878 --count 12
```

Twelve collars link to the herd and start reporting. Draw a boundary, send it, and watch the acks
come in and the herd walk into it. Add `--token <app token>` for a server on another machine.

## Build from source

Needs Rust (stable) and [bun](https://bun.sh).

```
cd ui && bun install && bun run build && cd ..
cargo build --release -p op-cli -p collar-sim     # target/release/openpasture, collar-sim
cargo test --workspace

bun run desktop:build                             # the macOS app, Apple Silicon
bun run desktop:release                           # universal .app and .dmg
```

The server embeds `ui/dist` at build time; if it is missing, the build runs bun for you.
Releases are cut by pushing a tag; see `docs/RELEASING.md`.

Docs: `docs/SELF-HOSTING.md` (running it for a farm), `docs/TEXTING.md` (Twilio, email, webhook,
push, the relay), `docs/API.md` (HTTP API, collar protocol, events, MCP tools), `docs/PLAN.md`
(design).

## Licence

The app is [AGPL-3.0](LICENSE). `op-protocol` and `op-geo` are Apache-2.0, so collar firmware
and other software can use the protocol and geometry without the AGPL.
