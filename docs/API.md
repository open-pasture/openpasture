# API

The contract between the UI, the crates, and collars. JSON everywhere. Geometry is
GeoJSON, `[longitude, latitude]`, WGS 84. Times are RFC 3339 UTC. IDs are prefixed
ULIDs (`farm_…`, `pad_…`, `herd_…`, `ani_…`, `col_…`, `bnd_…`, `dec_…`).

Errors: HTTP status plus `{ "error": "message" }`.

`/api/*` and `/mcp` need `Authorization: Bearer <app token>` (or `?token=`; the token is in
settings, shown in the app and printed by `openpasture token`) unless the request is local: from a loopback address, with a
`Host` of `127.0.0.1`, `localhost` or `[::1]` (any port), and no proxy headers
(`X-Forwarded-*`, `Forwarded`, `CF-Connecting-IP`, `X-Real-IP`, `Tailscale-*`). So traffic
through a tunnel (cloudflared, Tailscale Funnel) or a LAN address always needs the token, and a
DNS-rebinding page gets 401. Local requests that write (POST/PUT/PATCH/DELETE) or open the
WebSocket are refused (403) when `Origin` is another site; requests carrying the token are not
checked. Debug builds also allow CORS and `Origin` from the Vite dev server
(`http://localhost:5173`, `http://127.0.0.1:5173`). `/collar/v1/*` uses collar keys and
`/v1/decide` hosted keys, never the app token.

## Farm (op-core)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/state` | | `{ farm: Farm \| null, herds: Herd[], paddocks: Paddock[], settings: Settings }` |
| POST | `/api/farm` | `{ name, center: [lon, lat], timezone? }` | `Farm` |
| PATCH | `/api/farm` | partial `Farm` | `Farm` |
| GET POST | `/api/paddocks` | `{ name, geometry: Polygon }` | `Paddock[]` / `Paddock` |
| PATCH DELETE | `/api/paddocks/:id` | partial | `Paddock` / 204 |
| GET POST | `/api/herds` | `{ name, species, count, paddock_id? }` | `Herd[]` / `Herd` |
| PATCH DELETE | `/api/herds/:id` | partial (incl. `autonomy`) | `Herd` / 204 |
| GET POST | `/api/animals` | `{ tag, name?, herd_id, collar_id? }` | `Animal[]` / `Animal` |
| PATCH DELETE | `/api/animals/:id` | partial (not `removed_at`/`removed_reason`) | `Animal` / 204 |
| GET PUT | `/api/settings` | partial `Settings` | `Settings` |
| GET | `/api/secrets` | | `{ name, set: bool }[]` (never values) |
| PUT DELETE | `/api/secrets/:name` | `{ value }` | 204 |

```ts
Farm     { id, name, timezone, center: [lon, lat], created_at }
Paddock  { id, name, geometry: Polygon, area_ha, status: "resting"|"grazing"|"planned", notes?, grazed_until?, created_at,
           props?: { fsa_farm?, fsa_tract?, fsa_field?, … } }
Herd     { id, name, species: "cattle"|"sheep"|"goats", count, paddock_id?, autonomy: "propose"|"timer"|"auto", timer_minutes, created_at }
Animal   { id, tag, name?, herd_id, collar_id?, eid? /* 15 digits */, breed?, sex?: "female"|"male"|"castrated",
           born?: "YYYY-MM-DD", notes?, removed_at?, removed_reason?: "sold"|"died"|"culled"|"moved_off" }
Settings { brain: { id: BrainId, model?: string }, decision_time: "HH:MM",
           server: { bind: string, port: number, public_url?: string, app_token: string },
           units: "metric"|"imperial" }
BrainId  = "codex"|"claude"|"anthropic"|"openai"|"compatible"|"hosted"|"heuristic"
```

Secret names: `anthropic_api_key`, `openai_api_key`, `compatible_api_key`,
`compatible_base_url`, `hosted_api_key`, `hosted_url` (optional, default
`https://api.openpasture.dev`), `firecrawl_api_key` (the land provider key).

The farm's `timezone` comes from its `center` (offline lookup, `tzf-rs`) on create and
whenever a PATCH moves the centre; a body `timezone` is used only where the location has no zone.

`PUT /api/settings` is a JSON merge patch: `null` clears an optional field (e.g.
`{ "brain": { "id": "heuristic", "model": null } }`). Concurrent updates each merge into the
latest stored settings (one write transaction), so none is lost. A herd created in a resting
paddock marks that paddock `grazing`.

Autonomy follows the website's switch: `propose` waits for approval, `timer` applies after
`timer_minutes` unless the farmer stops it, `auto` applies at once. Changing a herd's
`autonomy` (or `timer_minutes` while on timer) also moves its open proposed MOVE: timer sets
`apply_at = now + timer_minutes`, auto sends it at once, propose clears `apply_at`.

## Collars and boundaries (op-ingest)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/collars` | `?herd_id` | `Collar[]` |
| POST | `/api/collars` | `{ name?, herd_id }` | `{ collar: Collar, key: string, endpoint: string, public_key: string }` (key shown once) |
| PATCH DELETE | `/api/collars/:id` | `{ name?, herd_id?, animal_id? }` (only these columns are written; `fw`, `caps`, `outside_since`, `parked_*` come from the collar and other routes) | `Collar` / 204 |
| GET | `/api/herds/:id/boundary` | | `BoundaryStatus` |
| POST | `/api/herds/:id/boundary` | `{ geometry: Polygon, warn_m?, hysteresis_m?, effective_at? }` | 201 `Move` (records a farmer decision and starts a move; see Moves) |
| GET | `/api/positions` | `?herd_id` | `Position[]` latest per collar |

```ts
Collar   { id, name, herd_id, animal_id?, last_seen?, battery? /* 0-1 */,
           boundary_version?, state: "inside"|"warning"|"outside"|"unknown", last_fix?: Fix,
           fw?, caps?: string[], outside_since?, parked_at?, parked_reason?: "charging"|"shelf"|"repair" }
Fix      { at, point: [lon, lat], accuracy_m, sats, cn0?, ttf_s? }
Position { collar_id, animal_id?, fix: Fix, state }
Boundary { id, herd_id, version, geometry: Polygon, warn_m, hysteresis_m, effective_at?,
           decision_id, created_at, collar_id? /* one collar's own, see Escapes */ }
BoundaryStatus { active?: Boundary, pending?: Boundary, proposed?: { decision_id, geometry },
           acks: { collar_id, version, status: "received"|"applied"|"rejected", reason?, at }[],
           move?: Move, escapes?: Escape[], staged?: Boundary[], slots?: SlotCount[] }
SlotCount { version, effective_at?, applied, stored, rejected, collars }
```

Boundary versions come from one sequence shared by every herd (herd A may hold v1, v3, v4 and
herd B v2); `active`/`pending` are still per herd. A collar moved to another herd starts over
(`boundary_version` cleared, state `unknown`); if the new herd's boundaries are all at or below
the version the collar held, they are stored again as new versions so the collar picks them up.
`POST /api/herds/:id/boundary` writes the farmer decision, the move and its first boundary
together, then supersedes the herd's open proposals and moves the herd to the paddock under the
target.

Collar state and `last_fix` only move with fixes newer than the current `last_fix`; late or
backfilled fixes are stored but don't move the collar or send `fix` events.

`outside_since` is the time of the first fix outside after the last one inside (as escapes count
it), cleared once a newer fix is back in (inside or warning) or the fence goes away. A parked
collar (`parked_at` set) still reports: its battery, health and `last_seen` are kept, but its
fixes and cues are not stored and it gets no fence or escape work. Each ack also updates
`collar_boundary_state` (the collar's highest acked version and that version's latest status),
so nothing scans `acks` for it.

### Moves: target and sweep

The farmer (or an approved decision) sets a **target**. The collars enforce the **active**
boundary. The server never sends an active boundary that leaves an animal outside it.

- If every tracked animal is already inside the target by at least `warn_m + 2` m, the target
  is sent as the active boundary directly and the move is `done` at once.
- Otherwise the server runs a **sweep**. Each step is an active boundary that contains every
  animal still in the sweep and the target, clipped to the old active boundary ∪ the target. Its
  back edge sits just behind the rearmost animal (relative to the direction from the herd toward
  the target), so only animals at the back line are in the warning zone and the cue pushes them
  toward the target. Sides tighten around the herd as it bunches. The next step goes out only
  once the collars report every animal in the sweep ahead of the next back line, and at most one
  step every 30 s. The last step is the target itself.
- An animal that doesn't move up for 5 minutes becomes a **straggler**. It's dropped from the
  sweep (the next step may leave it outside, and it is never cued there; see the collar rule)
  and listed on the move for the farmer.
- A new target replaces the running move. Stop keeps the current active boundary and ends the move.

Details. "Tracked" animals are collars in the herd with a fix from the last 10 minutes. The sweep
direction (herd centroid toward target centroid) is fixed for the whole move; when the herd
surrounds the target (its centroid is about on the target), the steps close in from every side
instead (the hull of the herd, buffered by `0.6 × warn_m`). A step goes out when the new back line
is at least `max(2 m, 0.3 × warn_m)` ahead of the last. The last step is sent when every animal in
the sweep is inside the target by `warn_m + 2` m, or when the back line has reached the target's
rear edge and every animal is at least 1.5 m inside it. Only animals holding the sweep up (behind
the next back line, or not held by the next step) run the 5-minute straggler clock; moving up 1 m
restarts it, and so does every step. Animals already outside the active boundary when a move
starts are listed as stragglers at once. When the old active boundary and the target don't touch
(paddocks drawn with a gap), the first step spans the convex hull of both, so the herd has a way
across. Every step is a new boundary version under the move's decision (`decision_id`), signed
with `herd_id` like any other. A move started with a future `effective_at` stages its first
boundary; the sweep waits while a staged boundary is pending. Sweeping moves carry on after a
restart. Stop answers 409 when no move is running.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/herds/:id/boundary` | as above; the geometry is the **target** | `Move` |
| POST | `/api/herds/:id/move/stop` | | `Move` (409 when none is running) |

```ts
Move { id, herd_id, decision_id, target: Polygon, status: "sweeping"|"done"|"stopped",
       step: number, remaining_m: number /* back line to target */, stragglers: string[] /* collar ids */,
       started_at, updated_at }
BoundaryStatus.move?: Move   // the running move, or the last one for 10 min after it ends
Event { type: "move", move: Move }   // on start, each step, each new straggler, done, stopped
```

Decisions: approving (or auto/timer applying) a MOVE starts a move with the decision's geometry as
the target, through the same path.

### Escapes: one animal's own boundary

An animal that stays outside its herd's active boundary gets a boundary of its own, so it can be
cued back without opening the paddock for the rest of the herd to follow it out.

- A collar whose state has been `outside` for 60 s (its own outside tone stops after 10 s) with a
  fix from the last 10 minutes starts an **escape**. Its collar is sent the herd's active
  boundary joined to a pen around the animal: the sweep planner's step for that one animal with
  the herd's boundary as the target. The animal sits in the pen's warning band at the back, so
  it is cued toward the herd; the pen reaches no further out than the animal.
- The pen closes in behind the animal like a sweep step: at most every 30 s, and only once the
  animal is `max(2 m, 0.3 × warn_m)` further along. It never gives up on its own; an animal that
  doesn't move stays held where it is.
- When the herd's boundary changes (a sweep step, a new target), the pen is rebuilt against it at
  once. An animal that gets out of its pen too gets a new pen where it is.
- It is **back** when the planner would send the target itself (inside the herd's boundary by
  `warn_m + 2` m, or past its near edge and 1.5 m in). Its collar then gets a copy of the herd's
  boundary with a new version (a collar never goes back to an older version); the copy carries
  the herd version it copies, and its ack is reported with that version in
  `BoundaryStatus.acks`. Herd boundaries staged at that moment are stored again above the copy.
- The farmer can **let it go**: the collar gets the herd's boundary, which leaves the animal
  outside and so uncued. It isn't given another escape until it has had a fix inside.
- The rest of the herd never sees a pen. A move's sweep leaves escaped animals out of its
  positions. Pens are boundary versions like any other, under the decision the herd is on, with
  `collar_id` set; `active`/`pending` only ever show the herd's.

A collar out on an escape is served only its own boundary on `/collar/v1/boundary`, and
`latest_version` in its report replies is its own. Its fixes keep their state against the herd's
boundary: `outside` means outside where the herd should be.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/collars/:id/escape/stop` | | `Escape` (409 when none is open) |

```ts
Escape { id, herd_id, collar_id, status: "returning"|"back"|"stopped",
         geometry?: Polygon /* the collar's own boundary now */, version?, step: number,
         remaining_m: number /* pen's back line to the herd's boundary */, started_at, updated_at, ended_at? }
BoundaryStatus.escapes?: Escape[]   // open ones, and those ended in the last 10 min
Event { type: "escape", escape: Escape }   // on start, each step, back, stopped
```

### Collar rule: only cue a crossing

A collar cues only when the animal goes from inside (or warning) to outside under the boundary
it holds. After boot or a new boundary the fence is unarmed; the first fix anywhere inside the
polygon (warning zone included) arms it, so sweep steps cue the rearmost animals at once. If a
new boundary leaves the animal outside it stays silent (state `outside`, no cue). Once the
collar has seen the animal outside, only a fix clear of the warning zone re-arms it, so an
animal walking back in isn't cued on its way. A crossing plays the outside tone (10 s limit)
and disarms. This is in the firmware
(`cue.c`), `op-geo`'s cue port, and `collar-sim`.

### Device endpoints (collars)

Per `../opencollar/protocol/README.md`. HTTP + JSON for V0. `Authorization: Bearer <collar key>`.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/collar/v1/report` | position report (`collar_id`, `boundary_version`, `fixes`, `cues`, `battery`, optional `health`) | `{ latest_version }` |
| GET | `/collar/v1/boundary?have=N` | | 204 if N is current, else the boundary command plus `sig` |
| POST | `/collar/v1/ack` | `{ command_id, version, status, reason?, at }` | 204 |

`sig` is an Ed25519 signature (base64) over the command's canonical JSON without `sig`.
The server's key pair lives in the data directory; the public key is given at link time.
The command carries `herd_id` (signed with the rest); a collar rejects a command for a herd
other than its own. Commands without `herd_id` (older servers) are accepted.

## Decisions and brains (op-engine, op-brain)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/decisions` | `?herd_id&limit` | `Decision[]` newest first |
| GET | `/api/decisions/:id` | | `Decision` |
| POST | `/api/herds/:id/decide` | | `Decision` with `status: "running"` (progress on `/api/live`) |
| POST | `/api/decisions/:id/respond` | `{ action: "approve"\|"reject"\|"modify", geometry?, note? }` | `Decision` (409 unless `proposed`) |
| GET | `/api/brains` | `?refresh=1` re-detects now | `Brain[]` |
| POST | `/api/brains/:id/test` | | `{ ok, detail, ms }`: one real decision on this farm's record, no MCP; `detail` is `ACTION (model)` or a one-line error |
| GET POST | `/api/brains/hosted/keys` | `{ label? }` | `HostedKey[]` / 201 `HostedKey & { key }` (key shown once, `oph_…`) |
| DELETE | `/api/brains/hosted/keys/:id` | | 204 / 404 |
| POST | `/v1/decide` | `{ context, instructions?, schema? }`, `Authorization: Bearer oph_…` | `DecisionOutput` |
| GET | `/api/knowledge` | `?q&limit` | `{ id, title, kind, body, source }[]`; empty `q` lists lessons then seed |
| GET | `/api/land/:paddock_id` | `?refresh=true` skips the 6 h cache. Land reports: the configured land provider when a key is set, otherwise open data | `{ paddock_id, report_id, source, as_of, cached, summary: string[], sections }` |
| GET | `/api/signals` | `?herd_id` | `Signals` |

```ts
Decision { id, herd_id, source: "brain"|"farmer"|"heuristic", brain?: BrainId, model?,
           status: "running"|"proposed"|"approved"|"applied"|"rejected"|"failed"|"superseded",
           action?: "STAY"|"MOVE"|"NEEDS_INFO", to_paddock_id?, geometry?: Polygon,
           reasoning?, confidence?, need?, inputs, apply_at?, boundary_id?, error?,
           created_at, responded_at?, outcome? }
Brain    { id: BrainId, name, available: boolean, signed_in: boolean, needs: string[] /* secret names */,
           models: string[], detail?: string }
HostedKey { id, label, created_at, last_used? }
DecisionOutput { action: "STAY"|"MOVE"|"NEEDS_INFO", to_paddock_id, geometry, reasoning, confidence, need, model }
Signals  { as_of, herd_id, current_paddock_id, position_source, herd_animal_units, feed_budget_days_current,
           behavior, risk_flags, assumptions,
           paddocks: { paddock_id, name, status, area_ha, current, rest_days, grazing_pressure, forage, recovery, risk_flags }[] }
```

A brain is ready when `available && signed_in`. `needs` lists every secret field the brain
reads, optional ones included (compatible: `compatible_base_url`, `compatible_api_key`;
hosted: `hosted_api_key`, `hosted_url`). `detail` is a short status such as "ChatGPT sign-in",
"Add API key", "Sign in with codex login".

**Responding.** `approve` sends a MOVE (STAY and NEEDS_INFO become `approved`); `reject`
never sends; `modify` needs `geometry`, sends the farmer's shape and keeps the brain's in
`inputs.proposed_geometry`. `inputs.farmer_response = { action, at, note?, geometry? }`.
A `note` becomes a farm lesson and appears in the next decision's context as an
`observations` entry with `source: "farmer-note"` (and in `history[].farmer_response`).
The app answers a NEEDS_INFO decision by posting the farmer's reply as `approve` + `note`,
then starting a new decision.

**Autonomy.** `propose` waits. `timer` sets `apply_at = now + timer_minutes` on a proposed
MOVE and the server sends it then unless it was answered. `auto` sends a MOVE at once.
STAY and NEEDS_INFO always wait. The daily decision runs for every herd with collars once
the farm's local time reaches `settings.decision_time` (within the following hour, once a day).

**Hosted brain.** `/v1/decide` is outside `/api`, so the app token does not apply; the
hosted key does. It runs this server's own configured brain: 401 bad key, 409 when this
server's brain is itself `hosted` or is `codex` (Codex's tools can read files on the host even
in its read-only sandbox, so outside prompts never reach it), 503 brain not set up, 502 brain
failed. Claude Code serves with no built-in tools, no MCP, `--restricted` and `--safe-mode`. Point another
server at it with the `hosted_url` and `hosted_api_key` secrets and brain `hosted`.

### Decision context

What the engine hands a brain (`DecisionRequest.context`, also the `context` of
`/v1/decide`). Any field may be `null` or empty.

```ts
{ as_of, farm: Farm, herd: Herd & { animal_units },
  autonomy: { mode, timer_minutes },
  current_paddock_id, position_source: "collar"|"farm_record"|"unknown",
  paddocks: { id, name, status, area_ha, geometry, rest_days, last_grazed, notes?, grazed_until? }[],
  candidate_paddock_ids: string[],
  boundary: BoundaryStatus /* with move: the target being swept into, if any */,
  collars: { count, reporting_24h, quiet: string[], low_battery: string[] /* < 0.2 */,
             states: { inside, warning, outside, unknown }, fixes_24h, cues_24h,
             dominant_paddock_id, paddock_fix_counts: Record<paddock_id, number> },
  positions: { collar_id, animal_id?, point, at, state, paddock_id? }[],
  signals: { as_of, herd_animal_units, rest_days, last_grazed, forage, recovery, grazing_pressure,
             feed_budget_days_current, behavior, risk_flags, risk_flags_by_paddock, assumptions },
  land_reports: Record<paddock_id, { report_id, source, as_of, sections }>,
  land_report_notes: Record<paddock_id, string>,
  knowledge: { id, title, kind, body, source }[],
  observations: { content, paddock_id?, at?, source: "farmer-note" /* dated, last 7 days */
                  | "paddock-note" /* the paddock's standing notes */ }[],
  history: { id, created_at, source, status, action, from_paddock_id, to_paddock_id,
             reasoning, confidence, need, farmer_response?, outcome? }[] /* last 10, newest first */,
  units }
```

"Where the herd is": the paddock holding at least half of the herd's collar fixes from the
last 24 h since its current boundary took effect, else `herds.paddock_id`.

## Analytics (op-analytics)

| Method | Path | Query | Returns |
| --- | --- | --- | --- |
| GET | `/api/tracks` | `collar_id?&herd_id?&from&to&max_points` | `Track[]` |
| GET | `/api/analytics/health` | `herd_id\|collar_id&from&to&bucket` | `CollarHealth[]` |
| GET | `/api/analytics/behaviour` | `herd_id&from&to` | `Behaviour[]` |
| GET | `/api/analytics/heatmap` | `herd_id&from&to&cell_m?&normalize?` | `[lon, lat, weight][]` |
| GET | `/api/analytics/pasture` | `herd_id?&from&to` (default 90 days) | `PaddockPasture[]` |
| POST | `/api/sql` | `{ query, limit? }` | `{ columns: string[], rows: any[][], ms, truncated? }` |
| GET | `/api/export` | `table&format=csv\|geojson\|parquet&from&to&collar_id?&herd_id?` | file download |

`from`/`to`: RFC 3339, `YYYY-MM-DD`, `now`, or relative (`-24h`, `-7d`, `-30m`, `-2w`); default
the last 24 h. `bucket`: `10m`, `1h`, `1d` or seconds; default the smallest step giving at most
150 buckets (at most 2,000 when `bucket` is given, else 400) over the span the collars can have data for (health clips `from` to when the first of
its collars was linked, at least ten minutes, and `to` to now), so a herd linked an hour ago gets
one-minute buckets. Values with no data behind them are `null`, never estimated.

```ts
Track        { collar_id, points: [lon, lat, t_unix_seconds][] }   // first fix per time bucket + the latest
HealthPoint  { t, fixes, fix_rate /* 0-1 */, acc_p50, acc_p95, sats, cn0?, ttf_s?, battery /* 0-1 */, cues }
CollarHealth { collar_id, name, herd_id, animal_id, bucket_s, cadence_s, summary: HealthPoint,
               ack: { version, status, reason, at } | null, points: HealthPoint[] }
Behaviour    { animal_id, collar_id, tag, name, fixes, distance_km, paddock_hours: Record<paddock_id, number>,
               outside_hours, days: string[], cues_per_day: number[], cues,
               learning: { slope_per_day, trend: "falling"|"rising"|"flat" } | null }
PaddockPasture { paddock_id, name, area_ha, status, grazing_days, rest_days, pressure /* AU-days/ha */,
               au_days, last_grazed, ndvi, herds: string[] }
```

Battery history is op-ingest's `health` table (one row per report). `ndvi` is the latest
imagery NDVI mean from the paddock's land reports (so only with the land provider key;
open data has no imagery). SQL is one read-only `SELECT`/`WITH`/`EXPLAIN` over
`fixes`, `cues`, `acks`, `boundaries`, `decisions`, `collars` (no `key_hash`), `animals`,
`paddocks`, `herds`, at most 10,000 rows, 20 s. A query loads at most 100,000 rows per record
table and the newest 500,000 hot (SQLite) rows per telemetry table, plus the Parquet days. Export `table` is any of those or `tracks`;
GeoJSON works for `fixes`, `cues`, `tracks`, `paddocks` and `boundaries`.

## Server (op-server)

| Method | Path | Returns |
| --- | --- | --- |
| GET | `/api/server` | `{ version, data_dir, bind, port, lan_url?, public_url? }` |
| GET | `/api/live` | WebSocket, server → client `Event` messages |
| POST | `/mcp` | MCP (streamable HTTP, stateless, protocol up to `2025-11-25`), tools listed and called as the caller's identity; `?scope=brain` lists only the brain tools. A brain run off a loopback URL gets its own token (`opb_…`, in memory, valid for that run only, opens `?scope=brain` only and lists and calls only the tools it was minted with) |

```ts
Event =
  | { type: "fix", collar_id, animal_id?, herd_id, fix: Fix, state }
  | { type: "cue", collar_id, at, level, margin_m, kind?: "warn"|"outside", ring? }
  | { type: "ack", collar_id, herd_id, version, status, reason? }
  | { type: "collar", collar: Collar }
  | { type: "boundary", herd_id, boundary: Boundary }
  | { type: "decision", decision: Decision }
  | { type: "move", move: Move }     // a move started, stepped, dropped a straggler, finished or stopped
  | { type: "escape", escape: Escape } // an escape started, stepped, ended or was stopped
  | { type: "decision_log", decision_id, line }   // brain progress, one line at a time
  | { type: "resync" }   // this socket fell behind and missed events: refetch state
  // @HUB
  | { type: "alert", alert: Alert }
  | { type: "message", message: MessageLog }   // managers and up only
  | { type: "feature", feature: MapFeature, deleted?: true }
  | { type: "animals_changed", herd_id? }
  // @HUB-UI
  // @E-lib
  // @E-srv
  // @J
  // @A-engine
  // @A-notify
  // @D
  // @K-animals
  // @K-files
  // @I
  // @B
  // @G
  // @P
  // @Q
  // @C
  // @F
  // @S
  // @A3
  // @H
  // @L
  // @M
  // @Z
```

Each socket gets only the events its identity may see: `message` events go to managers and up
(they carry phone numbers), everything else to every role.

A report publishes one `fix` event per collar (its newest new fix), not one per fix.

## MCP tools (op-engine)

Every tool call is logged at info (`op_engine::mcp: mcp tool call tool=…`).
Read tools for the brain and for any agent: `get_farm`, `list_paddocks`, `get_herd`,
`get_herd_positions`, `get_boundary_status`, `get_signals`, `get_land_report`,
`search_knowledge`, `list_decisions`, `get_decision`, `run_sql`. Write tools for outside
agents only: `propose_boundary` (records a decision; autonomy still applies).

<!-- @HUB -->

## Identity, people, shared records (op-core)

| Method | Path | Returns |
| --- | --- | --- |
| GET | `/api/me` | `{ role, via, user?: { id, name, phone?, phone_verified?: bool, email? } }` |

```ts
Role     = "viewer"|"hand"|"manager"|"owner"          // ordered
Via      = "local"|"app_token"|"user_token"|"brain"|"text"|"system"|"anonymous"
Actor    { via: Via, user_id?, name? }                 // who did something, stored on records
User     { id /* usr_… */, name, role: Role, phone? /* E.164 */, phone_verified_at?, email?, created_at, disabled_at? }
Severity = "info"|"warning"|"critical"
Finding  { code, severity: Severity, text, geometry?: GeoJSON, targets?: [kind, id][] }
Alert    { id /* alr_… */, kind, key, severity, status: "open"|"acked"|"resolved", herd_id?, title, body?,
           at?: [lon, lat], targets: [kind, id][], data, opened_at, updated_at, acked_at?, acked_by?: Actor,
           resolved_at?, resolved_by?: Actor, rolled_into? }
MessageLog { id /* ntf_… */, direction: "out"|"in", channel: "sms"|"whatsapp"|"email"|"webhook"|"relay"|"push",
           address, user_id?, kind: "alert"|"brief"|"reply"|"test"|"verify"|"inbound", text, subject?,
           status: "queued"|"sending"|"sent"|"delivered"|"failed"|"received"|"ignored", error?,
           alert_id?, decision_id?, provider_id?, attempts, created_at, updated_at }
MapFeature { id /* fea_… */, kind: "exclusion"|"water"|"gate"|"shade"|"hazard"|"road"|"neighbour_line"|"farm_boundary",
           name?, geometry: Point|LineString|Polygon, paddock_id? /* none = farm-wide */, notes?, props,
           active_from?, active_until?, created_at, updated_at }
```

Every request carries an identity. The app token and local requests are the owner
(`via: "app_token"` / `"local"`); a brain token on `/mcp?scope=brain` is the brain (a viewer that
lists and calls only its tools); `/collar/v1`, `/v1/*` and `/hooks/*` authenticate themselves.
`/hooks/*` paths that don't exist are JSON 404s. A handler that needs a role answers 401 without
credentials and 403 `{"error": "Your role can't do this."}` when the role is too low.

People (`users`) need no sign-in: someone who only texts is a user with a phone. Phones are stored
as E.164 (10 digits are a US number); changing a phone clears its verification. `users`,
`messages` and the other tables holding phone numbers are never readable through `/api/sql` or
export.

Map features: an exclusion is one polygon ring; water, shade and hazards are a point or a polygon
(a hazard point needs `props.radius_m`); a gate is a point; roads and neighbour lines are lines;
there is at most one farm boundary (409). A feature is active from `active_from` (inclusive) to
`active_until` (exclusive); either may be absent. Exclusions become boundary holes on sends whose
activation time falls in their window; the rest are checked before a send, never enforced.

Numbers people read (reports, texts, the brief) go through the farm's `settings.units`: metric
(ha, m, cm, kg, m²/hd, AU/ha) or imperial (ac, ft, in, lb, ft²/hd, AU/ac), rounded the same in
op-core and the UI: area one decimal (two below 0.1, none from 1,000), lengths whole (nearest 10
from 100), heights whole, mass and area per head three significant figures, density one decimal,
"," between thousands. The API is always SI.

Pasture history (`/api/analytics/pasture`) reads daily dwell from `paddock_days`: the rollup's own
days plus imported position history.

<!-- @HUB-UI -->
<!-- @E-lib -->
<!-- @E-srv -->
<!-- @J -->

## People, roles and sign-in (op-core, op-server)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/users` | | `Person[]`, enabled first, by name |
| POST | `/api/users` | `{ name, role, phone?, email? }` | 201 `Person` (no sign-in); 409 phone or email taken |
| GET | `/api/users/:id` | | `Person` / 404 |
| PATCH | `/api/users/:id` | `{ name?, role?, phone?, email?, disabled? }` (`null` clears phone, email) | `Person`; a new phone clears its verification; `disabled: true` also revokes every token and open link |
| DELETE | `/api/users/:id` | | 204 / 404; their tokens and links go too, records keep the name they stored |
| POST | `/api/users/:id/revoke` | | `Person`: every token revoked, open link dropped |
| GET | `/api/invites` | | `Invite[]` open (not accepted, not expired), newest first |
| POST | `/api/invites` | `{ user_id }` or `{ name, role, phone?, email? }` (adds the person now) | 201 `Invite & { code, url }`; `code` and `url` are shown once; replaces the person's open link |
| DELETE | `/api/invites/:id` | | 204 / 404 (open links only) |
| POST | `/api/invites/accept` | `{ code, label? }`, no token needed | `{ token, user: User }`; `token` (`opu_` + 64 hex) is shown once. 404 unknown or dropped link, 410 used, expired or person disabled, 429 after 5 tries a minute from one peer |
| GET | `/api/tokens` | | `TokenInfo[]` not revoked, newest first |
| DELETE | `/api/tokens/:id` | | 204 / 404; that browser gets 401 on its next request |
| PATCH | `/api/me/profile` | `{ name?, phone?, email? }` | `User` (your own; 404 when you aren't in People; role can't be changed here) |
| GET | `/api/me/tokens` | | your own `TokenInfo[]` |
| DELETE | `/api/me/tokens/:id` | | 204 / 404 |
| POST | `/api/me/signout` | | 204: revokes the token this request came with (400 without one) |

```ts
Person    = User & { tokens: number /* browsers signed in */, last_used?, invite_until? /* open link expires */ }
Invite    { id /* inv_… */, user_id, name, role: Role, phone?, email?, created_by?: Actor, created_at, expires_at, accepted_at? }
TokenInfo { id /* tok_… */, user_id, label, created_at, last_used?, revoked_at? }
```

**Sign-in.** No passwords. Adding a person gives them no sign-in: someone who only texts is a
person with a phone. A sign-in link is `{base_url}/#/join/<code>` (a 128-bit code in hex, valid
7 days, accepted once; the code rides in the URL fragment, which browsers don't send). Accepting
it returns a person token (`opu_…`) that the browser keeps and sends as
`Authorization: Bearer opu_…` (or `?token=` on the WebSocket), everywhere the app token works. A
person may have several tokens (one per browser); a new link for the same person signs in another
browser. Codes and tokens are stored as sha256 only. Tokens resolve through a 30 s cache;
revoking a token or a person's sign-in, or changing, disabling or removing a person, applies to
the next request. `last_used` is written at most once a minute. A revoked or unknown person token
is 401 even from this machine (it never falls back to the local owner). The app token and local
requests are the owner; when the owner added themselves to People (as an owner), they act as the
first enabled owner person (`/api/me` shows it, records name them).

**Roles** (`viewer < hand < manager < owner`), checked for every `/api` request before the route:

- viewer: `GET`/`HEAD` everywhere except the reads below; `/api/live`; MCP read tools.
  Every role may also use `/api/me` and `/api/me/*` and `POST`/`DELETE /api/push/subscriptions*`.
- hand: a viewer plus `POST /api/alerts/:id/ack`, `POST /api/alerts/:id/resolve`,
  `PUT /api/alerts/prefs/me`, `POST /api/herds/:id/move/stop`, `POST /api/collars/:id/escape/stop`,
  `POST /api/collars/:id/park`, `POST /api/collars/:id/unpark`, `POST /api/fleet/:id/fit-checks`,
  `POST /api/fleet/fit-checks`, `POST /api/paddocks/:id/heights`, `POST /api/feed-log`,
  `POST /api/herds/:id/check`; MCP read tools plus `ack_alert`, `resolve_alert`.
- manager: every other `/api` request except the owner's; `GET /api/messages` and
  `GET /api/alerts/prefs` are managers' reads; all MCP tools.
- owner: also `PUT /api/settings`, `/api/secrets*`, `/api/users*`, `/api/invites*` (except
  `accept`, which needs no sign-in), `/api/tokens*`, `/api/brains/hosted/keys*`, `/api/notify/*`,
  `/api/texting*`, `PUT /api/push/settings`, `POST /api/collars/:id/rekey`.

A role too low is 403 `{"error": "Your role can't do this."}`. `GET /api/settings` and
`GET /api/state` send `server.app_token: ""` to everyone but owners. `POST /api/sql` is a
manager's (it is a POST). MCP lists and calls tools as the caller: a viewer or hand never sees
`propose_boundary`, and calling it answers "Unknown tool".

**Who answered.** `POST /api/decisions/:id/respond` records the caller:
`inputs.farmer_response.by = Actor` (e.g. `{ via: "user_token", user_id, name: "Ana" }`, or
`{ via: "local" }` for the owner on this machine without a person), and the activity events the
answer causes (`decision.approved`, `decision.rejected`, `decision.applied`) carry the name in
`payload.by` ("owner" for the app token or a local request without a person). A proposal made
through MCP `propose_boundary` keeps its caller as `inputs.by`.
<!-- @A-engine -->
<!-- @A-notify -->
<!-- @D -->

## Map features (op-core)

| Method | Path | Returns |
| --- | --- | --- |
| GET | `/api/features?kind=&paddock_id=&active=` | `MapFeature[]` in the order they were stored. `active=true`: in effect now; `active=<RFC 3339>`: in effect then; absent: all, whatever their window |
| POST | `/api/features` | `{ kind, geometry, name?, paddock_id?, notes?, props?, active_from?, active_until? }` → 201 `MapFeature` |
| GET | `/api/features/{id}` | `MapFeature` |
| PATCH | `/api/features/{id}` | JSON merge patch → `MapFeature`. `null` clears a field (`paddock_id: null` = farm-wide, `active_until: null` = lasting); `id`, `kind` and the timestamps don't change |
| DELETE | `/api/features/{id}` | 204 |

Geometry per kind (400 otherwise, e.g. `{"error": "A gate is drawn as a point."}`): exclusion a one-ring
Polygon; water, shade and hazard a Point or a Polygon (a hazard point needs `props.radius_m` > 0);
gate a Point; road and neighbour line a LineString (≥ 2 points); farm boundary a Polygon.
Coordinates are stored to 7 decimals and polygon rings closed. A second farm boundary is 409
(`"The farm already has a boundary. Edit that one instead."`); an unknown `paddock_id` is 400;
`active_until` must be after `active_from` (400); names ≤ 200 and notes ≤ 2,000 characters.
Deleting a paddock deletes its features. Every create, change and delete publishes a `feature`
event (`deleted: true` on delete). Writes are manager and up.

MCP: `list_features` (read) `{ kind?, paddock_id?, active?: bool }` → `MapFeature[]`.

Place phrases in texts and replies (`op_core::place::describe`) name the nearest named gate,
water or shade in effect within 200 m: `"60 m N of east gate"`, `"at east gate"` (under 10 m),
`"in north pond"` (inside a water or shade area); otherwise the paddock: `"in P3"`,
`"60 m N of P3"`. Distances are in the farm's units.
<!-- @K-animals -->

## Animals and collar linking (K-animals)

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/animals/import/preview` | CSV text (≤ 5 MB; comma, semicolon or tab; UTF-8, UTF-16 or Windows-1252), `?herd_id` optional | `{ import_id, columns, mapping, rows, total, errors }` |
| POST | `/api/animals/import/:import_id/commit` | `{ mapping, herd_id }` | `{ created, updated, unchanged, total, errors }` (404 once the preview expired) |
| POST | `/api/animals/:id/remove` | `{ reason: "sold"\|"died"\|"culled"\|"moved_off", at? }` | `Animal` (409 when already removed) |
| POST | `/api/animals/:id/swap` | `{ collar_id }` | `Animal` |
| POST | `/api/collars/:id/park` | `{ reason: "charging"\|"shelf"\|"repair" }` | `Collar` |
| POST | `/api/collars/:id/unpark` | | `Collar` |
| POST | `/api/collars/bulk` | CSV rows `tag,collar name` with `?herd_id`, or `{ herd_id, items: [{ tag?, name? }] }` | 201 `{ batch_id, collars: [{ collar, key, endpoint, public_key, tag? }] }` (keys shown once) |
| POST | `/api/collars/:id/rekey` | | `{ collar, key, endpoint, public_key, tag? }` (owner) |
| POST | `/api/cards` | `{ items: [{ collar_id, key }] }` | `[{ collar_id, qr_svg }]` |

```ts
Mapping    = { tag: column, eid?, name?, breed?, sex?, born?, collar?, notes? }   // field → column name
RowError   { row /* spreadsheet row, header = 1 */, error }
```

`POST /api/animals` and `PATCH /api/animals/:id` also take `eid` (15 digits; spaces, dashes and dots
are dropped), `breed`, `sex` (`female`\|`male`\|`castrated`), `born` (`YYYY-MM-DD`, not in the future)
and `notes`. A tag names one animal on the farm per herd (409 `Tag 214 is already in this herd.`);
an EID names one animal for good, removed ones included (409 `That EID is on 214.`). A removed
animal frees its tag.

**Head count.** Once a herd has any animal rows, `herds.count` is its animals on the farm (not
removed), kept by every create, delete, herd change, import, remove and swap; `PATCH` of another
`count` on such a herd is 400 `Count follows the animals in this herd.`. Every such change publishes
`animals_changed { herd_id }`.

**Import.** The preview guesses the mapping from the header (Tag, Visual ID, Ear tag; EID, RFID,
ISO; Name; Breed; Sex, Gender; DOB, Birth date, Born; Collar; Notes, Comments), returns the first 20
rows as they are in the file and the rows the guessed mapping would skip (with `herd_id`, checked
against that herd too). Previews live in memory for 30 minutes. A commit creates animals whose tag
isn't in the herd and updates those whose tag is; an empty cell leaves the field as it is, so
committing the same file twice changes nothing. Sex reads F/female/cow/heifer, M/male/bull,
steer/castrated; birth dates read `2022-04-01`, `4/1/2022` (month first on a farm in a US time
zone, day first elsewhere), `4/1/22`, `01.04.2022` (always day first). A `collar` column names a
collar of the herd by name or id and puts it on the animal (a parked collar goes back on duty). A
row with any error is skipped and listed; the rest go in, in one transaction.

**Remove and swap.** Removing keeps the animal's row and fixes, takes its collar off and parks it
(`shelf`). A swap puts another collar of the same herd on the animal; the old one comes off and is
parked (`shelf`). Fixes carry `animal_id`, so both collars' fixes stay the animal's. Both write an
activity event (`animal.removed`, `collar.swapped`; `payload.by` is the person's name when known).

**Park.** A parked collar raises no alerts, isn't drawn or counted in slot counts, and its reports
keep only battery, health and `last_seen`. Parking forgets its fence state and stops an open escape
for it; parking again only changes the reason.

**Bulk linking.** CSV rows `tag,collar name` with or without a header (columns found by header
when there is one). An empty tag makes a spare collar; an empty name takes the tag, else
`Collar N`. All or nothing: any row to fix is 400 `{ error, errors: [{ row, error }] }` (no animal
with that tag in the herd, the animal already wears a collar, a repeated tag or name, a name
already used in the herd) and nothing is created. `endpoint` is the server's base URL +
`/collar/v1`. A new key (`rekey`, owner only) stops the old one at once.

**Cards.** `qr_svg` is an inline SVG QR code (error correction M) of the provisioning payload,
compact JSON in this order: `{"v":1,"c":"<collar id>","h":"<herd id>","k":"<collar key>","e":"<public url>/collar/v1","s":"<server public key, base64>"}`.
Keys only ever travel in request bodies; each must be the collar's current key (400 otherwise:
print again from a new key). Cards need `server.public_url` set to an https URL (409 otherwise),
because the endpoint is written into the collar and a LAN or tunnel address would strand it.

MCP: `list_animals` (read) `{ herd_id?, q?, removed? }` → `{ count, animals: [Animal & { herd, collar?: { id, name, state, battery?, last_seen?, position?, fix_at?, parked? } }] }`;
animals on the farm unless `removed` is true.
<!-- @K-files -->
<!-- @I -->
<!-- @B -->
<!-- @G -->
<!-- @P -->
<!-- @Q -->
<!-- @C -->
<!-- @F -->
<!-- @S -->
<!-- @A3 -->
<!-- @H -->
<!-- @L -->
<!-- @M -->
<!-- @Z -->
