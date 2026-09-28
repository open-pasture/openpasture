# openpasture app plan

Decided 2026-09-26, updated 2026-09-28. This is the Rust rewrite of `openpasture-agent-kit`: the software
that runs collars, stores their data, analyses it, and decides where the herd goes.

## Decisions

| Question | Decision |
| --- | --- |
| Relation to the Python kit | Full rewrite in Rust. The kit stays as a reference and is archived once its features are ported. Skills and the `seed/` knowledge corpus move over as-is. |
| First user | DIY builders and researchers: a few collars, raw data, room to tinker. |
| Field-ready target | One cattle operation: 250 head, 250 collars, a manager and hands who mostly text. |
| Platforms | Desktop (macOS, Windows, Linux) and the web. On a phone the same UI installs as an app over https. |
| UI | Tauri 2 shell, React + MapLibre GL UI, all heavy work in Rust. The same UI is served by the server as a web app. |
| Self-hosting | One server binary with embedded storage. Runs on a VPS, a Pi, or a farm PC. Our paid hosting runs the same binary. |
| Embedded mode | The desktop app can be the server: local database and collar ingest, reachable by collars through a tunnel. It can also connect to a remote server. |
| Licence | AGPL-3.0 for the app. Apache-2.0 for the protocol and geometry crates so collars and other software can use them without the AGPL. |
| Agent brains | Codex CLI (the user's ChatGPT subscription), Claude Code CLI (the user's Claude subscription), API keys (Anthropic, OpenAI, any OpenAI-compatible endpoint including Ollama), and the openpasture hosted brain (our subscription). |
| Analytics in V1 | Collar and fence health, animal behaviour, grazing and pasture, raw data export. |
| Texting, alerts, approvals | In the free app with the farm's own Twilio number, SMTP, a signed webhook or Web Push. The hosted relay (texts from openpasture's number) is paid. |
| People | Multi-user in the open app: owner, manager, hand, viewer; sign-in links, no passwords; people who only text need no sign-in. |

## Shape

```
collars ──(HTTP now, CoAP later)──▶ ingest ─▶ store ─▶ analytics
                                       ▲         │
                     boundary + ack ◀──┘         ▼
                                   engine ◀── brain (codex / claude / API / hosted)
                                     │
                                   alerts ◀──▶ people: texts, email, webhook, push out; Y, N, OK back
                                     │
                        axum server: REST, WebSocket, MCP, web UI
                                     │
                 desktop app (Tauri)   browser and phone   Claude / ChatGPT / coding agents
```

Everything above runs in one process. The desktop app links the server as a
library; `openpasture serve` runs the same thing headless.

## Workspace

```
openpasture/
  crates/
    op-protocol     collar protocol types, validation, signing                  Apache-2.0
    op-geo          rings, holes, projection, signed distance, geofence, cues    Apache-2.0
    op-core         Ctx, domain types, SQLite store, migrations, event bus, people, settings
    op-ingest       collar endpoints and keys, boundary dispatch, acks, moves, escapes, slots
    op-analytics    rollup to Parquet, DataFusion, SQL console, export, coverage, fleet, welfare
    op-brain        Brain trait and its backends, questions
    op-engine       decision cycle, signals, land reports, knowledge, strips, schedules, pre-send check, MCP tools
    op-alerts       alert rules, routing, channels, sender, texts in, relay, Web Push
    op-reports      paddock record, NRCS 528, organic season, lease head-days, welfare
    op-import       animal CSV, paddock files, position history, collar cards
    op-server       axum: merges every router, /api/live, static UI
    op-cli          the `openpasture` binary: serve, token, version
  tools/collar-sim  simulated collars that speak the real protocol (dev tool)
  apps/desktop      Tauri 2 shell, embeds op-server
  ui/               React + MapLibre + Vite, shared by desktop, web and phone
  skills/           markdown skills, ported from the kit
  seed/             knowledge corpus, ported from the kit
```

## Data layer

The record (farms, paddocks, herds, animals, collars, boundaries, decisions,
activity events) is relational and small. Telemetry (fixes, cues, health) is
append-only and large: one collar at 1 Hz is 86,400 fixes a day.

- **SQLite in WAL mode** holds the record and recent telemetry. One file, easy
  to back up, no server to run.
- **Daily Parquet segments** hold telemetry older than a few days, partitioned
  by farm and day. A researcher can open them directly in Python, R or DuckDB.
- **DataFusion** runs analytics across both, and backs an SQL console in the
  app. It is pure Rust, so the single binary stays small and builds fast.
- The domain model follows the kit's
  [`docs/domain.md`](https://github.com/open-pasture/openpasture-agent-kit/blob/archive/decision-layer/docs/domain.md):
  explicit entities plus an append-only activity event log with targets.
- The Convex store in the kit is not carried over. The hosted product uses the
  same SQLite and Parquet layout per farm.

## Collar ingest

Follows `opencollar/protocol/README.md`.

- **V0: HTTPS + JSON.** Matches the prototype checklist (JSON POST, telemetry,
  boundary download by version, ack). Easiest to debug with curl.
- **V1: CoAP over DTLS + CBOR** on LTE-M. Native to the nRF91 and the lowest
  power per message. Same logical messages, mapped one to one.
- **Per-collar keys.** Each collar gets a key when it is linked to a farm.
  Boundaries are signed (Ed25519) so a collar only accepts its own server's.
  This answers the protocol's open authentication question.
- **Reachability in embedded mode:** a built-in Tailscale Funnel or Cloudflare
  Tunnel option, or a LAN address for bench testing.
- **The simulator speaks the same wire protocol,** so everything can be built
  and tested without a collar, and a DIY builder can try the app on day one.

## Agent brains

One `Brain` trait: given the decision context and tools, return a structured
decision (`STAY`, `MOVE`, `NEEDS_INFO`, reasoning, confidence, boundary).

| Backend | How |
| --- | --- |
| Codex CLI | Runs `codex exec` with an output schema and our MCP server attached. Uses the user's own ChatGPT login. |
| Claude Code CLI | Runs `claude -p` with JSON output and our MCP server attached. Uses the user's own Claude login. |
| API keys | Direct calls to Anthropic, OpenAI, or an OpenAI-compatible URL (Ollama, vLLM, OpenRouter). |
| Hosted | Calls the openpasture API with an account key. The paid subscription. |
| Heuristic | The kit's no-LLM fallback, so decisions still happen with nothing configured. |

The CLI backends run the official CLIs as the user signed them in. We never read
or copy their tokens.

The same tools the brain uses are exposed through MCP and the CLI, so Claude,
ChatGPT, Codex or any agent can run the farm directly. Every boundary still
comes from a recorded decision (the kit's cloud-boundary rule).

## Analytics

| Area | V1 |
| --- | --- |
| Collar and fence health | Fix availability, accuracy, satellites, C/N0, time-to-fix, battery curves, cue counts, boundary ack status per collar. Feeds the V1 GNSS test plan. |
| Animal behaviour | Tracks with replay, heatmaps, distance walked, time per paddock, cues per animal over time (learning curve). Grazing and resting states once collars have an IMU. |
| Grazing and pasture | Grazing pressure, rest days, recovery, forage and NDVI from land reports, paddock history. |
| Raw data | CSV, GeoJSON and Parquet export, SQL console, documented schema. |

## Open source and paid

| Open (this repo, AGPL) | Paid (`openpasture-cloud`, private) |
| --- | --- |
| Everything a farm needs to run collars, store data, analyse it and decide: alerts, approvals by text, the morning brief, Web Push, people and roles, reports | Managed hosting, billing, the hosted brain, the hosted relay (texts from openpasture's number), backups, support tooling, custom features |

The cloud wraps this binary. It does not fork it.

## Milestones

1. **Scaffold.** Cargo workspace, `op-protocol` and `op-geo` with tests ported
   from the firmware host tests and the kit's `rings.py`, the simulator, CI.
2. **Collar loop.** HTTP ingest, store, live map, draw a boundary, send it,
   see the ack. First with the simulator, then with the V0 collar. Closes the
   prototype checklist's telemetry and boundary items.
3. **Collar analytics.** Health dashboards, tracks and replay, export, SQL console.
4. **Brains and decisions.** Brain trait and all backends, decision cycle port,
   MCP server, morning brief.
5. **Grazing analytics.** Land reports, knowledge search, paddock history and signals.
6. **Packaging.** Signed desktop builds, server binaries and a Docker image,
   self-hosting docs, embedded-mode tunnel.
7. **Field-ready.** 250 collars on one farm: people and roles, alerts and texting, exclusions,
   strip schedules staged on the collars, pre-send checks, animals and imports, reports, the
   welfare record, the phone layout and Web Push.

## Still open

- CoAP vs MQTT for V1 collars. Leaning CoAP. Decide after measuring power on V0.
- Trademark check on "OpenCollar" before anything is sold under the name.
