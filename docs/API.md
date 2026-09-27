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
<!-- @A-engine -->
<!-- @A-notify -->
<!-- @D -->
<!-- @K-animals -->
<!-- @K-files -->
<!-- @I -->
<!-- @B -->
<!-- @G -->
<!-- @P -->
<!-- @Q -->

## Questions and the morning brief (op-brain, op-engine)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/brief` | `?herd_id` (optional when the farm has one herd) | `Brief` (404 no such herd, 400 more than one herd and no `herd_id`) |
| POST | `/v1/ask` | `{ question, context?, max_chars? /* default 320, at most 2000 */ }`, `Authorization: Bearer oph_…` | `{ answer }` |

```ts
Brief { herd_id, lines: string[], text /* GSM-7, at most 480 characters */ }
```

**The brief** is written from the decision record, no LLM: the same record gives the same
brief. For today's decision (the herd's newest since the last `settings.decision_time` in farm
time, superseded ones left out) it gives the call (`Cows: MOVE to P4 (30.6 ac).`,
`Cows: STAY in P3.`, `Cows: NEEDS_INFO.`), where it stands (`Reply Y or N.` while it waits,
`Sends 07:40 unless you reply N.` on a timer, `Sent, 248/250 collars confirmed, 200 ft to go.`
once sent: collars not parked that applied its active boundary, and the sweep still left),
the one thing to check when the decision asks for it, then up to four reasons as the record
has them. When today's decision is still running, failed or missing, one line says so
(`Cows: no decision yet today.`). Then stale or missing data (`3 of 250 collars silent for a
day.`, `Herd position from the farm record, not collars.`, `Imagery for P3 is 20 days old.`,
`No field note in 7 days.`) and the lines other features add, in order. Numbers are in the
farm's units. `text` is the lines as one text: GSM-7 (curly quotes, dashes and accents made
plain, emoji left out), at most 480 characters, giving up the fourth and third reason first,
then stale-data lines, the second reason, other features' lines, the check and the first
reason; the call and where it stands always stay. MCP: `get_morning_brief { herd_id? }` (read).

**Questions** (`Brain::ask`, used by texting) get a short answer from a farm summary and read
tools, never `run_sql`: the Anthropic, OpenAI and compatible brains call up to 6 tools within
45 s; Claude Code reaches this server's `/mcp?scope=brain` with a token that lists and calls only
those tools; the hosted brain asks another server's `/v1/ask`; Codex and the heuristic don't
answer questions. Answers are plain text cut to `max_chars` at a sentence end. A compatible
server that refuses tools answers from the summary alone.

`/v1/ask` is the hosted side: the hosted key applies, and this server's own brain answers from
the `context` sent, with no tools. 401 bad key, 400 empty question, 409 when this server's brain
is itself `hosted`, 501 `{"error": "This server's brain doesn't answer questions."}` for Codex
and the heuristic, 503 brain not set up, 502 brain failed.
<!-- @C -->
<!-- @F -->
<!-- @S -->
<!-- @A3 -->
<!-- @H -->
<!-- @L -->
<!-- @M -->
<!-- @Z -->
