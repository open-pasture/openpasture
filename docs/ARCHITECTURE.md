# Architecture

How openpasture is built today, and where the collar link is heading. What exists is stated as
fact; what is being built is marked **planned** or **draft**. For the HTTP API, see
[`api/README.md`](api/README.md); for running it on a farm, [`SELF-HOSTING.md`](SELF-HOSTING.md).

## One process

```
collars ──(HTTPS + JSON, protocol v1)──▶ ingest ─▶ store ─▶ analytics
                                           ▲         │
                         boundary + ack ◀──┘         ▼
                                       engine ◀── brain (codex / claude / API / heuristic)
                                         │
                                       alerts ◀──▶ people: texts, email, webhook, push out; Y, N, OK back
                                         │
                            axum server: REST, WebSocket, MCP, web UI
                                         │
                   desktop app (Tauri)   browser and phone   Claude / ChatGPT / coding agents
```

Everything above runs in one process. `openpasture serve` runs it headless; the desktop app
links the same server as a library and runs it in-process. There is no second service, queue or
database server.

openpasture is one client of the collar protocol. The protocol, its test vectors and the
firmware live in [opencollar](https://github.com/open-pasture/opencollar); any other software
that speaks the protocol can run the same collars.

## Workspace

| Path | What it is | Licence |
| --- | --- | --- |
| `crates/op-protocol` | Collar protocol types, validation, command signing | Apache-2.0 |
| `crates/op-geo` | Rings, holes, projection, signed distance, geofence, cues | Apache-2.0 |
| `crates/op-core` | `Ctx`, domain types, SQLite store and migrations, event bus, people, settings, secrets, the data-dir lock | AGPL-3.0 |
| `crates/op-ingest` | Collar endpoints and keys, report ingest, boundary and config dispatch, acks, slots, moves, escapes, pre-send checks | AGPL-3.0 |
| `crates/op-analytics` | Telemetry rollup to Parquet, DataFusion, SQL console, export, coverage, fleet care, welfare | AGPL-3.0 |
| `crates/op-brain` | The `Brain` trait and its backends; questions | AGPL-3.0 |
| `crates/op-engine` | Decision cycle, signals, land reports, knowledge search, strips and schedules, MCP tools | AGPL-3.0 |
| `crates/op-alerts` | Alert rules, routing, channels, texts in and out, the relay client, Web Push | AGPL-3.0 |
| `crates/op-reports` | Paddock record, NRCS 528, organic season, lease head-days, welfare | AGPL-3.0 |
| `crates/op-import` | Animal CSV, paddock files, position history, collar cards | AGPL-3.0 |
| `crates/op-fleet` | Units from the factory to the farm: registry, factory tests, provisioning, claim, firmware rollout, herd network, collar console ([`api/fleet.md`](api/fleet.md)) | AGPL-3.0 |
| `crates/op-server` | axum: merges every crate's router, `/api/live`, the embedded web UI | AGPL-3.0 |
| `crates/op-cli` | The `openpasture` binary: `serve`, `token`, `version` | AGPL-3.0 |
| `apps/desktop` | Tauri 2 shell; embeds `op-server` | AGPL-3.0 |
| `ui/` | React + MapLibre + Vite; one UI for desktop, browser and phone | AGPL-3.0 |
| `tools/collar-bridge` | A real collar on USB serial, talking to a server; captures and replays exchanges (bench) | AGPL-3.0 |
| `simulation/collar-sim` | Simulated collars and throwaway simulated farms, speaking the real protocol. Test tool, never pointed at a real farm, never linked into the product | AGPL-3.0 |
| `skills/`, `seed/principles/` | The daily decision's instructions and the grazing principles corpus, embedded in `op-engine` | AGPL-3.0 |

`op-protocol` and `op-geo` depend on no AGPL crate, so firmware and other clients can use them
under Apache-2.0. CI checks that on every pull request.

Each crate owns its routes and merges them into the server; `op-core` owns the shared state
(`Ctx`), and nothing below `op-server` knows about HTTP framing beyond its own router.

## Data

Everything lives in one data directory: `OPENPASTURE_DATA_DIR`, else the platform data
directory, or `--data-dir`.

| File | What |
| --- | --- |
| `openpasture.db` | SQLite in WAL mode: the record (farms, paddocks, herds, animals, collars, boundaries, decisions, people, activity events) and recent telemetry |
| `telemetry/` | Daily Parquet segments for older telemetry, by table and day. Open them directly in Python, R or DuckDB |
| `secrets.json` | API keys and other secrets, mode 0600 |
| `knowledge-index/` | The search index over the seed corpus and the farm's lessons; rebuilt when missing |
| `skills/<name>/SKILL.md` | Optional: a farm's own copy of a skill wins over the built-in one |

One server per data directory: `Ctx::open` takes a lock on the directory and refuses to start
a second server on it. A copy of the stopped directory is a full backup.

DataFusion runs analytics across SQLite and Parquet and backs the SQL console. It is pure Rust,
so the binary stays a single file.

## Desktop app

The macOS app (Apple Silicon and Intel) runs the server in-process, bound to `127.0.0.1`, and
shows one window on the server's URL, so the UI and the API are same-origin. Quitting shuts the
server down. Updates come from GitHub releases, signed with the updater key and checked before
they install (`RELEASING.md`).

Windows and Linux desktop builds are **planned**; the server runs on Linux, macOS and (built in
CI) Windows today.

To reach collars and phones from outside, put the server behind an https address you run
yourself: a reverse proxy, Tailscale or a Cloudflare tunnel (`SELF-HOSTING.md`). A built-in
tunnel is **planned**.

## Brains

One `Brain` trait: given the decision context and tools, return a structured decision (`STAY`,
`MOVE`, `NEEDS_INFO`, reasoning, confidence, target).

| Backend | How |
| --- | --- |
| Codex CLI | `codex exec` with an output schema and openpasture's MCP server attached, under the user's own ChatGPT sign-in |
| Claude Code CLI | `claude -p` with JSON output and the MCP server attached, under the user's own Claude sign-in |
| API keys | Anthropic, OpenAI, or any OpenAI-compatible URL (Ollama, vLLM, OpenRouter) |
| Hosted | Another openpasture server decides with its own brain (`POST /v1/decide`, a key it issued); openpasture's paid hosting runs this |
| Heuristic | No LLM, so decisions still happen with nothing configured |

The CLI backends run the official CLIs as the user signed them in; openpasture never reads or
copies their tokens.

Every boundary a collar receives comes from a recorded decision or a person's action. One
safety gate on the server that every sender's boundary must pass (farm edge, roads, hazards,
a person's confirmation) is being built.

## Collar link

### v1, today: HTTPS + JSON

Collars post reports to `/collar/v1/report`, fetch boundaries and configs by version, and ack
them. Boundaries and configs are signed with the server's Ed25519 key; a collar accepts only
its own server's. Each collar has its own bearer key, issued when it is linked to a herd. The
spec and test vectors are in opencollar's `protocol/`; `op-protocol` implements them and CI
checks its vectors are byte-identical to the firmware's.

On the bench, `tools/collar-bridge` carries the same exchanges over a collar's USB serial
console, until the collar has its own network link.

### v2, draft: LoRa with anchor collars

The production collar is LoRa-first. Most collars send only LoRa, the bare minimum: position,
fence state, battery. A few anchor collars in each herd also carry cellular; they take turns
being the herd's link to the server, pass boundaries and moves down over LoRa, and pass the
herd's reports up. Every collar still takes its own GNSS fix and makes its own fence decision.

Protocol v2 is openpasture's own open protocol, written in opencollar's `protocol/v2/`
(**draft**): transport-neutral signed commands, a device key made on the collar at
manufacture, claim and handover without opening the collar, signed firmware, and compact
frames for LoRa. `op-fleet` holds the server side of units, provisioning and the herd network.

## Simulation

`simulation/` holds everything that fakes the world: `collar-sim` speaks the real protocol as
any number of collars, and builds throwaway simulated farms to test against. It is a separate
crate the product never links, and it never points at a real farm. The product itself has no
mock or demo mode.

## Open source and paid

The app in this repository is AGPL-3.0 and has everything a farm needs to run collars, store
and analyse their data, and decide: alerts, approvals by text with the farm's own Twilio
number, the morning brief, Web Push, people and roles, reports. Managed hosting, the hosted
relay (texts from openpasture's number) and the hosted brain are a paid service that runs this
same binary; it wraps the binary rather than forking it.

## Performance at 250 collars

The field-ready target is one farm with 250 collars reporting every few seconds. The notes on
write batching, WAL checkpointing and analytics at that scale are in `docs/API.md` (sections
"Analytics at 250 collars" and "250 collars") and move here when the API docs are split.
