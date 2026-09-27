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
           action?: "STAY"|"MOVE"|"NEEDS_INFO"|"HOLD" /* HOLD: strip schedules (S) */, to_paddock_id?, geometry?: Polygon,
           reasoning?, confidence?, need?, inputs, apply_at?, boundary_id?, error?,
           created_at, responded_at?, outcome? }
Brain    { id: BrainId, name, available: boolean, signed_in: boolean, needs: string[] /* secret names */,
           models: string[], detail?: string }
HostedKey { id, label, created_at, last_used? }
DecisionOutput { action: "STAY"|"MOVE"|"NEEDS_INFO"|"HOLD", to_paddock_id, geometry, reasoning, confidence, need, model }
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
  units,
  schedule /* S: the herd's strip schedule while it runs, see "Strip schedules" */ }
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
`fixes`, `cues`, `health`, `acks`, `boundaries`, `decisions`, `collars` (no `key_hash`), `animals`,
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
  | { type: "ack", collar_id, herd_id, version, status, reason?, code? }   // code: protocol v1 reject code, only with rejected
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
  // @P  (/api/live only, never on the bus; they replace fix, ack and cue on the socket)
  | { type: "positions", herd_id, items: PositionItem[] }   // per herd every 500 ms
  | { type: "ack_batch", herd_id, items: AckItem[] }
  | { type: "cue_batch", herd_id, items: CueItem[] }
  // @Q
  // @C
  // @F
  // @S
  | { type: "schedule", schedule: Schedule }   // made, changed (staged, opened, skipped, held, retimed), paused, resumed or ended
  // @A3
  // @H
  // @L
  // @M
  // @Z
  // @X1  (/api/live only: one farm window's batches together, see "Seams between streams")
  | { type: "batch", events: Event[] }
```

Each socket gets only the events its identity may see: `message` events go to managers and up
(they carry phone numbers), everything else to every role.

A report publishes one `fix` event per collar (its newest new fix), not one per fix. On the
socket, `fix`, `ack` and `cue` arrive as `positions`, `ack_batch` and `cue_batch` (see "Live feed
at herd scale").

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

## Protocol v1 on the server (op-ingest)

The collar protocol v1 (holes, slots, collar-scoped boundaries, cue kinds, episodes, signed
configs) as the server speaks it. Firmware 0.1 collars (no `caps`) keep working: every new field
is optional, and they get what they can hold.

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/herds/:id/slots` | | `HerdSlots` |
| GET | `/api/collars/:id/slots` | | `CollarSlots` |
| GET PUT | `/api/collars/config` | `CollarsConfig` (each 10-3600) | `CollarsConfig` (PUT gives every collar that takes configs a new version) |
| POST | `/collar/v1/report` | position report, see below | `{ latest_version, config?: ConfigCommand }` |
| GET | `/collar/v1/boundary?have=&free=&free_bytes=` | | 204, or the boundary command for this collar |
| POST | `/collar/v1/ack` | `{ command_id, version, status, reason?, code?, at }` | 204 |

```ts
CollarLimits { outer, holes, hole_vertices, total, slots, slot_bytes }   // LEGACY 64/0/0/64/2/0, V0 128/16/32/384/16/24576, V1 … 32/262144
HeldSlot     { version, copy_of? /* herd version it copies */, status: "applied"|"received"|"rejected", effective_at?, reported_at, code? }
CollarSlots  { collar_id, fw?, caps?: string[], limits: CollarLimits, slots: HeldSlot[],
               config?: { version, herd_id?, endpoint?, report_s, poll_s, fast_report_s?, fast_poll_s?, fast_until?, refused?: true },
               parked?: true, escaped?: true }
HerdSlots    { counts: SlotCount[] /* the herd's active and staged versions */, collars: CollarSlots[] }
CollarsConfig { report_s: 60, poll_s: 60, fast_report_s: 10, fast_poll_s: 10 }   // setting `collars.config`
BoundaryStatus.staged: Boundary[]   // every staged version still alive, version order
BoundaryStatus.slots: SlotCount[]   // per active/staged version: applied, stored, rejected of `collars`
BoundaryStatus.acks[].code?         // the reject code of a collar's latest ack
```

**Every herd boundary is prepared** before it is stored (farmer draw, applied decision, each
sweep step, reissue for a moved collar): the shape is validated and fitted to the strictest limits
among the herd's collars that hold holes (V0 when none report caps). Outer rings only shrink and
holes only grow. An invalid shape is 400 with a sentence, e.g. "Holes need 13 m between them and
from the edge." (gap `2·warn_m + 2 m` plus 0.5 m server slack, in the farm's units). Missing
`warn_m`/`hysteresis_m` default to 5 m and 1 m. Holes are allowed: `POST /api/herds/:id/boundary`
takes a Polygon with inner rings. Sweep steps carry the target's holes, and holes of the previous
boundary that still lie whole inside the step with the gap.

**Downloads are per collar.** The command for a collar is fitted to its caps and limits: `holes`,
`collar_id` and `cue_mode` only when in its caps. A collar with no caps (firmware 0.1) gets the
outer ring fitted to 64 vertices and no holes; the server-side fence for that collar uses the same
ring, so `state` matches what the collar enforces. Selection (§3.3): its boundary set (the herd's,
with its own copies after an escape; only its pen while out on one) split at now by the
activation rule (the highest version whose `effective_at`, or receipt, has passed is in effect; a
staged version is dead once a higher one takes effect at or before it); versions it refused for
good are dropped (every code except `slots_full`, and a rejection without a code); then the
version in effect if newer than `have`, else the lowest staged one above `have` when `free` > 0
and its record (`192 + 8 × vertices` bytes) fits `free_bytes`. A collar that sends no `free`
(firmware 0.1) has its slots less what its acks say it holds. `latest_version` in report replies is
the highest version of that set it hasn't refused.

**Reports** (§3.8) may carry `device { fw, caps, limits, config_version, config_reject }` (stored
on the collar: `fw` and `caps` show on `Collar`, limits in `CollarSlots`), `slots` (the complete
list; replaces the collar's applied/received rows), `episodes` (stored once per collar and start),
cue `kind`/`ring`/`dur_ms`/`boundary_version`, fix `hdop`/`boundary_version`, and health
`fix_attempts`, `fix_ok`, `cell { rsrp_dbm, rsrq_db, snr_db, mode, band, cell_id, tac }`,
`still_s`, `tilt_deg`, `temp_c`, `battery_v`, `charging`, `uptime_s`, `reset`. Cues without a kind
(firmware 0.1) are stored with `kind` NULL; the live `cue` event carries `outside` when
`margin_m` < 0, else `warn`. Acks store `code` on the ack and in `collar_boundary_state`; between
reports (and for firmware 0.1) acks keep the collar's slot rows: `applied` drops every lower
version, as the collar does.

**Configs** (§3.4): each collar with the `config` cap has one signed `ConfigCommand` (`cfg_…`,
version per collar). It gets a new version when its herd changes (at once on the PATCH), when
`server.public_url` changes (the `endpoint`, only when it is `https://`; seen on the collar's next
report), when a move starts for its herd or an escape for it (a fast window: `fast_until` = the
estimated end, at 4 m/min of `remaining_m` kept between 5 and 30 min, plus 10 min; extended while
the move runs once under 5 min is left; never cut short), and when `collars.config` changes. A
report whose `device.config_version` is lower (or absent) gets it in the reply; a version the
collar refused (`config_reject`) isn't sent again, and one above the server's (a restored
database) is overtaken by a new version.

**Escapes** keep the herd boundary's holes in the pen, open a fast window for the collar, use
`outside_since`, and end with copies for that collar alone: the herd's active boundary and each
staged one (`collar_id` + `copy_of`, same `effective_at`), so the rest of the herd downloads
nothing. The activation watcher announces a staged boundary only if it is in effect when its time
comes.

Tables: `collar_slots(collar_id, version, status, effective_at, reported_at, code)`,
`episodes(id epi_…, collar_id, herd_id, animal_id, start_t, end_t, start_at, end_at,
boundary_version, ring, cues, max_level, min_margin_m, outcome)`, `collar_config(collar_id,
version, body, updated_at, reject_version, reject_code)`; boundary indexes `(herd_id, collar_id,
version)`, `(effective_at)`, `(version)`.
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

## Alerts (op-alerts)

| Method | Path | Returns |
| --- | --- | --- |
| GET | `/api/alerts?status=&herd_id=&from=&to=&limit=` | `Alert[]`. `status`: `open` (unacked), `acked`, `resolved`, `all`; absent = open and acked, critical first, then newest. `from`/`to` (RFC 3339) bound `opened_at`; `limit` 1–1000, default 100 |
| GET | `/api/alerts/{id}` | `Alert` |
| POST | `/api/alerts/{id}/ack` | `Alert`; hand and up. Stops re-notification and escalation; acking again returns it unchanged; resolved → 409 |
| POST | `/api/alerts/{id}/resolve` | `Alert`; hand and up. Stays closed while its cause lasts; resolved → 409 |
| GET | `/api/alerts/rules` | `{ rules: RuleView[], policy: Policy, configured: string[], person_channels: ("sms"\|"whatsapp"\|"email")[] }` |
| PUT | `/api/alerts/rules` | same; manager and up. Body `{ rules?: { <kind>: Partial<RuleConfig> }, policy?: Partial<Policy> }` (merge; `null` puts a rule's number back to its default and clears quiet hours) |
| GET | `/api/alerts/prefs` | `PersonPrefs[]`, every person; manager and up |
| GET | `/api/alerts/prefs/me` | `PersonPrefs` of the caller's person; 404 when the sign-in isn't a person |
| PUT | `/api/alerts/prefs/me` | `PersonPrefs`; hand and up. Body: JSON merge patch of `AlertPrefs` |
| PUT | `/api/alerts/prefs/{user_id}` | `PersonPrefs`; owner only |

```ts
RuleConfig { enabled: bool, severity: Severity, after_min?: number, threshold?: number, notify: bool }
RuleView   = RuleConfig & { kind, sentence /* "Collar silent for {n}" */, unit: "min"|"%"|"m"|"", cadence_s, default: RuleConfig }
Policy     { renotify_every_min: 30, renotify_max: 3, escalate_after_min: 15, group_window_s: 60, rollup_min: 4,
             herd_silent_share: 0.5, clear_after_min: 2, start_grace_min: 20, critical_window_s: 10,
             quiet_start?: "HH:MM", quiet_end?: "HH:MM" /* the farm's, farm time */ }
AlertPrefs { channels: ("sms"|"whatsapp"|"email")[] /* ["sms"] */, min_severity: Severity /* "warning" */,
             herds?: string[] /* absent = every herd */, muted_kinds: string[], quiet_start?, quiet_end? /* absent = the farm's */,
             critical_in_quiet: bool /* true */, on_duty: bool /* false */ }
PersonPrefs = AlertPrefs & { user_id, name, role: Role, sms_opt_out: bool, updated_at? }
```

Rules (`kind`, default severity, number, notify): `escaped` an open escape (critical) · `outside` outside
the herd's boundary ≥ 5 min with no escape running and not let go on this trip out (warning; critical
when the collar is silent too) · `silent` no report for max(20 min, 3 × its median report interval
over 24 h); held for `start_grace_min` after a server start (warning) · `herd_silent` more than
`herd_silent_share` of a herd's reporting collars silent, at least two (critical; takes in that herd's
`silent` alerts) · `low_battery` < 20 % (warning, no texts; in the brief) · `boundary_not_applied` the
herd's boundary in effect ≥ 10 min and the collar holds an older one (or rejected it), not escaped and
not silent (warning) · `decision_waiting` a proposal unanswered 30 min, or a timer decision at once
(warning; `data.code` is the 4-digit approval code) · `move_stalled` a sweeping move with no step for
15 min (warning; `data.staged` when it waits on a staged boundary) · `stragglers` a move left animals
behind (info) · `drop_off` every fix (sampled every 5 min) within 4 m of their median for 240 min
(warning) · `gps_degraded` median accuracy over the last 10 min worse than 10 m, or no fix for 10 min
while reports arrive (info, no texts). Parked collars and removed animals never alert.

Keys are `<kind>:<subject id>`. Four or more collar alerts of one kind in one herd at once (`rollup_min`)
are one alert `<kind>:herd:<herd id>` ("31 outside P3", `data.count`, `data.members`), which keeps its
members until the last clears; members already open resolve with `rolled_into`. A key gone for
`clear_after_min` resolves by itself (`resolved_at` without `resolved_by`); back within that time it
keeps its row; back after it resolved it opens a new row. `decision_waiting` resolves as soon as the
decision is answered. `data` carries what the texts need: `label`, `herd`, `paddock`, `since`, …

Rules run on a 10 s tick (each on its `cadence_s`: `drop_off` 300 s, `low_battery` and `gps_degraded`
60 s) and early, after 2 quiet seconds, on bus `escape`, `decision`, `move`, `ack` and `boundary`
events; never on `fix` or `collar`.

Notifications are queued as `messages` (kind `alert`, `alert_id`, `decision_id` for decisions) for the
sender to deliver. A person gets an alert when its severity is at least theirs, its herd and kind are
theirs, and they can be reached over a configured channel: sms and whatsapp only to a verified phone
that hasn't texted STOP; with no Twilio of the farm's own, sms goes over the relay (channel `relay`,
address the phone); email only over the farm's own SMTP (the relay can't prove an address is the
person's); WhatsApp only with an approved template. When anyone matching is on duty the first send goes
only to them. Warnings wait `group_window_s` and go as one text per kind and herd ("3 outside P3: 214
031 118"); critical waits `critical_window_s` (a breakout of 250 is one text); info never pushes. Quiet
hours (the person's, else the farm's) hold warnings until they end and let critical through unless
`critical_in_quiet` is off. Unacked critical alerts are sent again every `renotify_every_min` up to
`renotify_max` times and escalate every `escalate_after_min` to matching people of the next role up not
yet told (hand → manager → owner). The farm webhook (channel `webhook`) gets every notified alert once.

Texts are GSM-7, at most 160 characters, names cut to fit, numbers in the farm's units:
`214 outside P3, 200 ft N of east gate, 6m. Reply OK to ack` · `31 outside P3 since 06:12. Reply OK to
ack` · `Cows: 180 of 250 collars silent 25m. Check coverage or the server` · `Cows: move to P4 (30.6 ac,
3 d)? Reply Y or N. Code 4821`.

MCP tools: `list_alerts` (read; `status?`, `herd_id?`, `limit?`), `ack_alert` and `resolve_alert` (hand;
`id`). The morning brief's `attention` line lists what doesn't text: "Battery low: 031 14%, 118 16%.
GPS weak: 207".

<!-- @A-notify -->

## Texting and delivery (op-alerts)

Farm side. `/api/notify/*` is the owner's; `/api/messages` is for managers and up (it shows phone
numbers).

| Method | Path | Returns |
| --- | --- | --- |
| GET | `/api/notify/channels` | `Channels` |
| PUT | `/api/notify/channels` | `ChannelsPatch` → `Channels`; 400 with the reason (a relay that refused says why). When the relay becomes how this server texts (it turns on or gets a new key or URL while the farm has no Twilio SMS, or Twilio goes while it is on), phones the relay hasn't verified for this key lose `phone_verified_at`, so Verify (a code from the relay) shows beside them again; phones it has keep theirs and get the dead-man flag by role |
| POST | `/api/notify/test` | `{ channel, to? }` → `{ ok, detail }` — sends now; without `to` Twilio checks the account and the relay lists its recipients |
| POST | `/api/notify/verify` | `{ user_id }` → `{ via: "sms"\|"relay", verified?: true }`; 400 no phone · 409 already verified or nothing can text · 429 within 30 s · 502 the provider's words |
| POST | `/api/notify/verify/confirm` | `{ user_id, code }` → `{ verified: true, phone_verified_at }`; 400 wrong or no code · 410 expired · 429 after 5 tries |
| GET | `/api/messages` | `?direction=in\|out&limit=&from=&to=&user_id=` → `MessageLog[]`, newest first (limit 100, at most 1,000) |
| GET/PUT | `/api/notify/hosting` | `Hosting` (PUT is a merge patch) |

```ts
Channels = {
  sms: { from? /* E.164, or a Messaging Service SID MG… */ },
  whatsapp: { from?, template_sid? /* HX…, one {{1}} body variable, used for alerts and briefs; replies go as text. Without it WhatsApp isn't offered for alerts or briefs (only replies within 24 h) */ },
  email: { host?, port /* 587 */, user?, from?, tls: "starttls"|"tls"|"none" },
  webhook: { url? },
  relay: { enabled /* only after the relay answered GET /v1/notify/recipients with 200 */, checked_at? },
  twilio_api_base /* "https://api.twilio.com" */,
  secrets: { name: "twilio_account_sid"|"twilio_auth_token"|"smtp_password"|"webhook_secret"|"hosted_url"|"hosted_api_key", set }[],
  configured: ("sms"|"whatsapp"|"email"|"webhook"|"relay")[],   // what can send now
}
ChannelsPatch = merge patch over the config (null clears) + { secrets?: { [name]: value | null }, relay?: { enabled } }
Hosting  = { enabled /* false */, per_key_minute /* 30 */, per_key_day /* 500 */, deadman_after_min /* 15 */ }
```

Anything that wants to reach a person enqueues a message (`op_core::messages::enqueue`); the sender
delivers it. It claims at most 10 queued messages at a time and 4 per channel, and a message is
sent once however many senders run. A send that may work later goes back in the queue: Twilio,
email and relay at 5 s, 30 s and 2 min; webhooks at 1 s, 5 s and 25 s; then `failed`. A provider
this server can't connect to at all (the farm's internet or DNS is down, the connection refused)
never saw the message, so that isn't counted as a try: the message stays `queued` and is tried
again after 5 s, then as often as every minute, for up to 6 h (then `failed`, "… can't be reached.
Gave up after 6 h."). An alert's text or email that had to wait (a minute or more) isn't sent once
its alert has resolved (`failed`, "Resolved before it could be sent."). A 4xx from
Twilio fails at once with Twilio's words (`"Twilio 21211: The 'To' number … is not a valid phone
number."`). Twilio texts are `sent` and read back at 30 s, 2 min and 10 min until Twilio says
delivered (`delivered`) or undelivered (`failed` with the reason), so failed deliveries show
without a public URL. A message for a channel that isn't set up fails ("SMS isn't set up.").

- **Twilio**: `POST {twilio_api_base}/2010-04-01/Accounts/{sid}/Messages.json`, basic auth, form
  `To`, `From` (or `MessagingServiceSid`), `Body`. WhatsApp uses `whatsapp:` addresses and, with a
  template and for anything but a reply, `ContentSid` + `ContentVariables {"1": text}`.
- **Email**: the farm's SMTP server, plain text, subject from the message (default "openpasture").
- **Webhook**: `POST url` with `{ "type": "alert", "alert": Alert, "text" }` for an alert (else
  `{ "type": "message", "message": MessageLog }`) and
  `x-openpasture-signature: t=<unix>,v1=<hex HMAC-SHA256(webhook_secret, "<t>.<body>")>`. Check it
  over the raw body, e.g. `printf '%s.%s' "$t" "$body" | openssl dgst -sha256 -hmac "$secret"`, and
  refuse an old `t`. A 2xx is `delivered`; 408, 429 and 5xx are retried.
- **Relay**: `POST {hosted_url}/v1/notify` with `Authorization: Bearer <hosted_api_key>` (the
  hosted brain's URL and key; the URL defaults to `https://api.openpasture.dev`). The message id is
  the idempotency key, so a retry is sent once. An address with `@` goes as email, else SMS. A
  text that asks the person about a decision (the decision's own text, a brief that asks, a LATER
  reminder, a reply naming what waits) goes with `prompt: true`. People are offered, and alerts
  and briefs go, by the relay only as SMS: the relay texts only addresses proven to it, and the farm
  can prove phones, not email addresses, so email needs the farm's own SMTP.

**Phone verification.** A person's phone gets texts only once it is proven by a 6-digit code:
through the farm's own SMS, else through the relay (which texts its own code). The code text is
the first text a number gets: `openpasture code 123456. Reply STOP to opt out.` Codes work for 10
minutes and 5 tries, one new code per 30 s; they are stored hashed together with the phone they
went to (a code sent to an old number never proves a new one) and the message log keeps the text
with the code masked. Changing a phone clears its verification. The relay texts only phones proven
to it for this server's key, so when the relay becomes how the farm texts (turned on, a new key or
URL, or the farm's own Twilio removed) phones it hasn't proven lose their verification here and
Verify (a relay code) shows beside them again; phones it has keep theirs, with the dead-man flag set
by role. A phone the relay later refuses as not verified (403 "That recipient isn't verified.") is
unverified the same way.

Relay host side (a server with `notify.hosting.enabled`, texting for the `oph_` keys it issued from
its own channels):

| Method | Path | Returns |
| --- | --- | --- |
| POST | `/v1/notify` | `{ idempotency_key, channel: "sms"\|"whatsapp"\|"email", to, text, subject?, kind?, prompt? /* the text asks about a decision */ }` → 202 `{ id, status: "queued"\|"duplicate" }` |
| POST | `/v1/notify/recipients` | `{ channel, to, deadman? }` → 202 `{ status: "sent"\|"verified" }` (texts or emails a code) |
| POST | `/v1/notify/recipients/verify` | `{ channel, to, code }` → 200 `{ verified: true }`; 404 no code · 400 wrong · 410 expired · 429 after 5 tries |
| GET | `/v1/notify/recipients` | `[{ channel, to, verified_at?, deadman }]` for the calling key |

Every `/v1/notify*` call: 401 without a key this server issued, 403 while hosting is off. `POST
/v1/notify` also: 403 recipient not verified, 409 when this server itself sends through a relay
(loop guard), 503 when it can't send that channel, 429 over `per_key_minute` or `per_key_day`
(codes count too). A repeated idempotency key is answered `duplicate` and sent once.

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

## Paddock files and position history (op-import)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| POST | `/api/import/paddocks/preview` | multipart `file` (≤ 20 MB): GeoJSON, KML, KMZ, or a zipped shapefile | `PaddockPreview` |
| POST | `/api/import/paddocks/commit` | `{ import_id, keep: number[], names?: (string\|null)[] }` | 201 `{ paddocks: Paddock[] }` |
| POST | `/api/import/positions/preview` | multipart `file` (≤ 64 MB): CSV, GPX or GeoJSON points; optional fields `mapping` (JSON) and `zone` | `PositionPreview` |
| POST | `/api/import/positions/{id}/preview` | `{ mapping?, zone? }` (the same file read again) | `PositionPreview` |
| POST | `/api/import/positions/{id}/commit` | `{ mapping?, zone?, animals?: Record<label, animal_id \| null> }` | 201 `PositionCommit` |
| GET | `/api/import/positions` | | `PositionImport[]` (newest first) |
| GET | `/api/import/positions/{id}` | | `PositionImport & { days: ImportedDay[] }` |
| DELETE | `/api/import/positions/{id}` | | 204 (its points and days go too) |
| GET | `/api/import/positions/tracks` | `animal_id?&import_id?&from?&to?&max_points?` (default 2000) | `ImportTrack[]` |

```ts
Draft           { name, layer?, geometry: Polygon, area_ha, props?: { fsa_farm?, fsa_tract?, fsa_field? } }
PaddockPreview  { import_id /* imp_… */, file, drafts: Draft[], errors: string[] }   // errors: polygons left out, one sentence each
Mapping         { tag?, time?, lat?, lon?, accuracy? }        // CSV column or GeoJSON property per field; no tag = one animal per file
ImportLabel     { label, points, from, to, animal_id?, tag? } // matched by tag, EID, collar name, then a numeric tag without leading zeros
PositionPreview { import_id, file, source: "csv"|"gpx"|"geojson", columns?: string[], mapping: Mapping, rows?: string[][] /* first 20 */,
                  total, points, labels: ImportLabel[], tracks: { label, points: [lon, lat, t_unix_seconds][] }[] /* ≤ 200 each */,
                  needs_zone: boolean, zone, errors: string[] }
PositionImport  { id, file_name, source, zone?, fixes, animals, from?, to?, created_by: Actor, created_at }
PositionCommit  { import: PositionImport, duplicates, skipped: string[] /* labels left out */, errors: string[] }
ImportedDay     { date /* YYYY-MM-DD UTC */, animal_id, paddock_id /* "" = outside every paddock */, fixes, dwell_s }
ImportTrack     { animal_id, points: [lon, lat, t_unix_seconds][] }   // first point per time bucket + the last, like Track
```

Paddock files: every polygon (and each part of a multipolygon) is a draft; holes stay holes. Zips
are searched at any depth, so a John Deere Operations Center export with nested folders and several
layers gives drafts from every polygon layer (points and lines are skipped, as are `__MACOSX/`
shadows). Names come from the attributes `NAME`, `FIELD_NAME`, `FIELD` (and similar), else the KML
Placemark name, else `Field <CLU number>`, else the layer name. FSA numbers come from
`FARM_NBR`/`FARMNBR`, `TRACT_NBR`/`TRACTNBR` and `CLU_NBR`/`CLUNBR`/`FIELD_NBR`. Shapefile
projections are read from the `.prj` (ESRI or OGC WKT 1): geographic WGS 84 or NAD83, Transverse
Mercator (UTM and state plane TM zones) and Lambert Conformal Conic with one or two standard
parallels (e.g. Iowa North/South, EPSG 26975/26976 in metres and 3417/3418 in US survey feet), in
metres, US survey feet or feet. NAD83 is taken as WGS 84 (they differ by under 2 m). Anything else
is refused with the projection, datum or unit named. A shapefile with no `.prj` is read as longitude
and latitude only when its coordinates fit. GeoJSON is WGS 84; an old `crs` member naming a UTM or
Iowa state plane EPSG code is projected. At most 2,000 polygons per file and 20,000 corners per ring.
A preview waits 30 minutes for its commit; a commit uses it up. Commit needs the farm.

Position history: CSV columns are guessed from the headers (tag, time, lat, lon, accuracy) and can
be changed with `mapping`; GPX tracks are labelled by their names (the animal's tag); GeoJSON Points
take time and tag from properties. Times with an offset (RFC 3339, `…-05:00`, `Z`, ` UTC`) or unix
seconds/milliseconds are exact; times without one are read in `zone` (default the farm's time zone),
and `needs_zone` says so. A point at `0,0` or out of range is an error row. Each animal keeps one
point per instant (`duplicates` counts the rest; a file with nothing new is 409). Commit stores the points in `imported_fixes` and
each animal's daily dwell per paddock in `imported_paddock_days` (today's paddocks, the rollup's
30-minute gap rule), so pasture history's rest days and last grazed include it; the rollup never
touches either table. Points are written in transactions of 5,000 so a large file never holds the
database long. After a commit or a delete: `animals_changed` for each herd concerned.

<!-- @I -->

## Reports (op-reports)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/reports` | | `{ id, title }[]` |
| GET | `/api/reports/:id` | `from?&to?&herd_id?&format=json\|csv` | `ReportDoc`, or one CSV file |
| GET PUT | `/api/reports/settings` | merge patch of `ReportInputs` | `ReportInputs` |
| GET POST | `/api/feed-log` | `herd_id?&from?&to?` / `{ herd_id, date, kg_dm, kind?, note? }` | `FeedEntry[]` (newest first) / `FeedEntry` |
| PATCH DELETE | `/api/feed-log/:id` | partial | `FeedEntry` / 204 |
| GET | `/api/leases` | | `Lease[]` (leases of existing paddocks) |
| GET PUT DELETE | `/api/leases/:paddock_id` | `{ landowner, rate_per, rate_amount, currency?, season_from?, season_to?, notes? }` | `Lease` / `Lease` / 204 |

```ts
ReportDoc     { id, title, farm, from: "YYYY-MM-DD", to: "YYYY-MM-DD", herd_id?, generated_at,
                header: [label, value][],          // Farm, Operator, FSA farm, Dates, Herd: only those known
                sections: ReportSection[], notes: string[] /* method lines */, signatures: string[] /* signature-line labels */ }
ReportSection { title, columns: { key, label, unit?, decimals? /* places a number column prints with */ }[],
                rows: (string|number|null)[][], totals?: (string|number|null)[] }
ReportInputs  { operator?, fsa_farm?, au: { cow: 1.0, bull: 1.35, pair: 1.3, weaned_calf: 0.5 },
                herds: Record<herd_id, { mean_weight_kg?, intake_pct: 2.5, mix?: { cows, bulls, calves, pairs: bool } }> }
FeedEntry     { id /* fed_… */, herd_id, date: "YYYY-MM-DD", kg_dm, kind /* default "hay" */, note?, created_by?: Actor, created_at }
Lease         { paddock_id, landowner, rate_per: "acre_season"|"head_day"|"au_day"|"aum"|"pair_month", rate_amount,
                currency /* ISO 4217, default "USD" */, season_from?, season_to?, notes?, updated_at }
```

Report ids: `paddock_record` (Paddock grazing record), `nrcs_528` (NRCS 528 grazing record),
`organic_season` (Organic grazing season), `lease_head_days` (Lease head-days). `from`/`to` are
farm-local days, both included; the default is January 1 of `to`'s year to today. A report never
counts past now. Values are in the farm's units (`settings.units`), rounded; each column's `unit`
names it (`ac`/`ha`, `AU/ac`/`AU/ha`, `lb`/`kg`, `%`, or a currency). `null` is an empty cell.

CSV (`format=csv`, `text/csv`, attachment `<id>-<from>-<to>.csv`): the title row and the header
rows (`label,value`), a blank row (one empty cell), then each section: its title row, the column row (`Label (unit)`),
the rows, the totals row (first cell `Total`), a blank row; then `Notes` and one note per row.
Numbers have no thousands separators and keep their column's `decimals`.

History comes from triggers, not the API: `herd_history` records each herd's count and paddock
whenever either changes (and its creation and deletion), `paddock_geometry_history` each paddock's
shape, area and name. On upgrade both are backfilled once: occupancy from applied MOVE decisions
(the time the activity log says the move was applied, else the response or creation time; where a
herd started from its first move's `from_paddock_id`, or for a farmer-drawn move the one paddock
that move marked `grazed_until`), head counts at the count on upgrade day, which the report notes say. A grazing event is a herd's stay in
one paddock; head is the count on the day in and head-days follow every count change inside it.
Stocking density is AU on the day in over the paddock's area then. Rest before in is the time since
any herd last left the paddock. Collar dwell (`paddock_days`) adds a "Collar days" column where it
exists. Animal units per head: the herd's mix with the `au` factors when a cattle herd has one
(with `pairs` a cow and her calf are one head at the pair factor), else `cattle 1.0`, `sheep 0.2`,
`goats 0.15`.

- `paddock_record`: every event (paddock, FSA field when present, herd, in, out, days, head, AU,
  head-days, AU-days, stocking density, rest before in), then a line per paddock.
- `nrcs_528`: field, FSA farm/tract/field (only those present; one shared FSA farm goes in the
  header), area, dates in and out, kind and number, AU, days, AUD, rest period; signature lines
  Operator and NRCS planner. Columns openpasture doesn't measure are left out.
- `organic_season`: days on pasture (farm days the herd was in a paddock) against 120; dry matter
  from pasture against 30 % only for herds with `mean_weight_kg` whose feed log has entries in the
  season's first and last week (a 0 kg entry counts): needed = head-days × weight × `intake_pct`,
  from pasture = needed − the feed log's dry matter.
- `lease_head_days`: a section per landowner, a row per leased paddock: dates (the lease season
  inside the report's dates), head-days, AU-days, AUM (AU-days ÷ 30.4), pair-months (pairs × days ÷
  30.4, when a herd's mix has pairs), rate and amount. `acre_season` is a flat rent per area for the
  season, owed when the season meets the report's dates; its `rate_amount` is per hectare, like
  every area in the API (the UI shows and takes it per acre on imperial farms). Signature lines:
  Operator and each landowner.

The feed log's `kg_dm` is dry matter in kg; `date` is the farm-local day. `POST /api/feed-log` is
open to hands; editing and deleting entries, leases and report settings need a manager.
MCP: `get_report` (read) `{ id, from?, to?, herd_id? }` returns the `ReportDoc`.

<!-- @B -->

## Map layers, measured heights (op-engine)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/layers/paddocks` | | `{ as_of, paddocks: PaddockLayer[] }` from the record and cached land reports only (never fetches) |
| GET | `/api/paddocks/:id/heights` | `?limit` (default 50, at most 500) | `Height[]` newest first |
| POST | `/api/paddocks/:id/heights` | `{ height_cm, residual_cm?, at? }` (hand and up) | 201 `Height` |

```ts
PaddockLayer { paddock_id, grazing?: true /* a herd is in it now */, rest_days?, last_grazed?,
               ndvi?, ndvi_at? /* YYYY-MM-DD of the imagery */,
               drought?: { category: "D0"|"D1"|"D2"|"D3"|"D4"|null /* null: not in drought */ },
               flood?: { in_floodplain: boolean, zone?, risk?: "medium"|"high" /* 3-day forecast */ } }
Height       { id /* hgt_… */, paddock_id, at, height_cm, residual_cm?, by: Actor, created_at }
```

A field is absent when nothing is known. Rest days count from when any herd last grazed the
paddock (applied moves, collar fixes, rolled-up and imported history, `grazed_until`; a paddock
with a herd in it now is 0). NDVI, drought and flood come from the newest cached land report:
they need the land provider key (open data has only weather), so without it they are absent.

A height is measured in the paddock (cm, over 0 and at most 300; `residual_cm` is what was left
behind, at most `height_cm`; `at` defaults to now and can't be in the future). `by` is who recorded
it. The newest height measured in the last 21 days replaces the imagery estimate in the grazing
signals; older ones stay listed but no longer count.

Changes to existing shapes (all additive):
- `Signals.paddocks[]` gains `grazing_days` (days this paddock's forage feeds the herd: 60 % of
  standing forage above a 3 inch residual at 11.8 kg DM per AU a day, capped at 365) and
  `last_grazed`.
- `forage` (in `/api/signals` and the decision context) carries `source: "measured"` with
  `height_cm` and `measured_at` when a height counts. When the paddock's land report shows snow
  deeper than 2 cm or a 7-day mean air temperature under 5 °C, imagery forage is withheld:
  `height_inches`, `available_kg_dm_per_ha` and `source` are null and `reason` is `"snow"` or
  `"dormant"`. A measured height still counts.
- The open-data weather section gains `current.snow_depth_cm` and `history[].temp_mean_c`.
- `POST /api/farm` sets `settings.units` from the farm's time zone: imperial in US zones, metric
  elsewhere. Only at creation; moving the farm later leaves the units alone.
<!-- @G -->

## Coverage and fleet care (op-analytics)

| Method | Path | Query or body | Returns |
| --- | --- | --- | --- |
| GET | `/api/coverage` | `metric=accuracy\|fixes&from&to&cell_m=10&herd_id?` (default the last 7 days) | `Coverage` |
| GET | `/api/fleet` | `herd_id?&collar_id?` | `FleetRow[]` |
| GET | `/api/fleet/{collar_id}/fit-checks` | | `FitCheck[]`, newest first |
| POST | `/api/fleet/{collar_id}/fit-checks` | `{ checked_at?, notes? }` or no body (hand) | 201 `FitCheck` |
| POST | `/api/fleet/fit-checks` | `{ collar_ids: string[], checked_at?, notes? }` (hand; a chute day) | 201 `FitCheck[]` |
| GET | `/api/fleet/settings` | | `{ fit_check_days: 30 }` |
| PUT | `/api/fleet/settings` | `{ fit_check_days }` (1–365; manager) | the same |

```ts
Coverage  { metric: "accuracy"|"fixes", cell_m, unit: "m"|"ratio",
            size?: [dlon, dlat],                 // degrees one cell spans; absent before any fix is counted
            cells: [lon, lat, value, n][] }      // each cell at its centre
FleetRow  { collar_id, name, herd_id, tag? /* the animal wearing it */, battery? /* 0-1 */,
            trend_pct_day?,                      // percentage points a day, last 7 days since the last charge, 2+ days of data
            days_left?,                          // at that trend: only when falling, 3+ days of data
            fit_checked_at?, fit_due_at,         // due = last check (else when the collar was added) + fit_check_days
            last_seen?, parked: boolean,
            daily: (number|null)[] }             // mean battery of each of the last 14 UTC days, oldest first, today last
FitCheck  { id /* fit_… */, collar_id, checked_at, by?: Actor, notes? }
```

Both read only the day tables op-analytics keeps: `coverage_days` (per herd, UTC day and 10 m
cell: fixes, an accuracy histogram of 8 buckets `<1, <2, <3, <5, <8, <12, <20, ≥20 m`, fixes
expected and fixes that came) and `battery_days` (per collar and UTC day: min, max, mean and last
battery). Neither ever reads `fixes` or `health`. A background task rewrites the days that changed
(new rows, or a Parquet day file that gained rows) a minute after start and then every 10 minutes
(less often when a run is slow, at most hourly); its first run covers every day there is data for,
Parquet included. Today is the partial day so far.

- `accuracy`: each cell's median fix accuracy in metres from the histogram; `n` is the fixes in it.
- `fixes`: fixes that came ÷ fixes the collar's cadence called for (its median gap that day), 0–1.
  A fix that never came counts where the animal was before the gap; a gap longer than 6 h means
  the collar was off and counts nothing. `n` is the fixes called for.
- Cells with `n` under 5 are left out. `cell_m` is a multiple of 10 up to 1,000: blocks of 10 m
  cells. Cells are counted from a fixed origin (the farm's centre when the first fix is counted),
  so a cell is the same ground every day.
- A fit check records who made it; the collar's next check is due `fit_check_days` after its latest
  check. Each check is also an activity event `fleet.fit_checked`.

MCP (read, viewers): `get_coverage` `{metric?, from?, to?, cell_m?, herd_id?}` returns a
`Coverage` (at most the 500 weakest cells, with `truncated: <cells there were>` when cut);
`get_fleet` `{herd_id?}` returns `{ fit_check_days, collars: FleetRow[] }`.

<!-- @P -->

## Live feed at herd scale (op-server, P)

`/api/live` sends the bus's `fix`, `ack` and `cue` events coalesced per herd into one
`positions`, one `ack_batch` and one `cue_batch` (each only when it has items), gathered for the
whole farm: 500 ms after the first of them for any herd they go out together, as one message (a
`batch` when there is more than one, see "Seams between streams"). At 250 collars that is about
one message a second, at most two batched ones however many herds are live, instead of dozens. Server-side subscribers (alerts, schedules) still get every single event.

```ts
PositionItem { collar_id, animal_id?, fix: Fix, state, battery? /* 0-1 */, last_seen? }  // newest fix and telemetry per collar
AckItem      { collar_id, version, status: "received"|"applied"|"rejected", code?, reason? }  // latest per collar
CueItem      { collar_id, at, level, margin_m, kind?: "warn"|"outside", ring? }              // every cue, in order
```

A `collar` event goes out on its own only when the collar's JSON minus `last_seen`, `battery`,
`last_fix`, `state`, `outside_since` and `boundary_version` changed (renamed, relinked, parked,
moved herd, fields added later). A report's telemetry rides in `positions`; a reported boundary
version with no ack rides in `ack_batch` as `applied`. A collar with no fix yet has no position
to send, so its telemetry-only changes wait for a refetch. Every other event goes out at once,
in bus order; batches can arrive after events published later in the same 500 ms. Each message
is serialized once for all sockets; a socket drops what its identity may not see; a socket (or
the coalescer) that falls behind gets `resync`. A socket opened with a person's token closes
within 5 s once that token is revoked, the person disabled or their role changed; the browser
reconnects as whoever it is now. A client's close gets a close back.

## Analytics at 250 collars (op-analytics, P)

- The hourly rollup streams a day to Parquet per collar in `(collar_id, t)` order, 65,536 rows a
  row group, merging an existing file for that day (a row in both is kept once), and sums pasture
  dwell on the stream. Rows leave SQLite in write transactions of at most 5,000 rows with a pause
  after each, so reports keep landing (a 4.3 M-fix day: memory under 50 MB, no report waits
  250 ms). While those deletes run, a rolled day is in both places: the SQL console, exports and
  health counts can count its rows twice until they finish; tracks and pasture don't.
- `health` rolls up like `fixes` and `cues` and is a table for `/api/sql` and export; battery
  history in `/api/analytics/health` reads both.
- `/api/tracks` seeks each collar's first fix per time bucket by index in SQLite and reads only
  the track columns of Parquet days (row groups outside the range or collar skipped). 250
  collars over a 4.3 M-fix day, `max_points=300`: under a second from SQLite, about 2 s from
  Parquet in a debug build.
- `/api/analytics/pasture` is as of the end of the range (or now): no day or fix after it
  counts, `last_grazed` is the last grazing day up to it (before `from` too, from the daily
  summaries) and `rest_days` runs to it. A day the end falls inside counts whole once rolled.
- `OPENPASTURE_DB_POOL` sets the SQLite pool (1-256, default 16).
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

## Strips and layouts (op-engine)

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| POST | `/api/strips/preview` | `{ paddock_id, herd_id?, orientation_deg, width_m? \| count? \| days?, head?, warn_m? }` | `StripPreview` |
| GET POST | `/api/layouts` | `?paddock_id=` / `{ paddock_id, herd_id?, name?, orientation_deg, width_m? \| count? \| days?, head?, warn_m? }` | `Layout[]` / `Layout` (201) |
| GET PATCH DELETE | `/api/layouts/:id` | PATCH `{ name }` | `Layout` / 204 |
| POST | `/api/layouts/:id/apply` | `{ herd_id?, head? }` | `AppliedLayout` |
| POST | `/api/paddocks/:id/copy` | `{ name?, offset_m?: [east, north] }` | `Paddock` (201) |

```ts
StripParams   { orientation_deg, width_m?, count?, days?, head?, warn_m? }   // exactly one of width_m, count, days
StripFacts    { geometry: Polygon, area_ha, grazeable_ha, days? }
StripPreview  { paddock_id, strips: StripFacts[], width_m, depth_m, head, animal_units,
                forage_kg_dm_per_ha?, forage_source?, warn_m }
Layout        { id /* lay_… */, paddock_id, name, params: StripParams, strips: Polygon[], created_by: Actor, created_at, updated_at }
AppliedLayout = StripPreview & { layout: Layout }
```

Strips are parallel bands across the paddock. `orientation_deg` is the compass bearing they
advance toward: 0 = strips run east-west and advance north (strip 1 is the southernmost), 90 =
they run north-south and advance east. `width_m` cuts bands of that width from the start (the
last keeps what is left), `count` cuts that many equal bands, and `days` picks the width that
gives that many days per strip. A band thinner than two warning zones and the collar's gap
between them (`2·warn_m + 2.5` m, 12.5 m at the default 5 m) joins its neighbour, so the result
can hold fewer strips than asked. On a concave paddock a band can fall into pieces; each piece is
its own strip, in order across the band, and a thin wedge joins the piece it shares the longest
cut with. The strips always cover the whole paddock; holes stay holes. More than 200 strips is a
400. `width_m` in the answer is the band width used (for `count`, depth ÷ count).

`grazeable_ha` is the strip less the exclusions in effect now (farm-wide and the paddock's).
`days` = forage × `grazeable_ha` ÷ (animal units × 11.8 kg DM a day), to 0.1 d, where forage is
the paddock's standing forage above the residual from its grazing signals (a cached land report,
or a measured height when one is recorded; never fetched here). Without forage or animals there
are no days, and sizing by `days` is 400. Head is `head`, else the herd's count; animal units
follow the herd's species (cattle when no herd). Exclusion holes are not cut into strips: sending
a strip is the farmer drawing that boundary (`POST /api/herds/:id/boundary`), which applies them.

A layout keeps its settings and its strips. Sized by `days`, it also keeps the `width_m` that
chose, so its strips stay put when the forage changes. `apply` lays it on its paddock now: the
stored strips with today's grazeable ground and days for the herd (or `head`, else the head it
was sized for); if the paddock changed shape since, the strips are cut again from the same
settings and stored. Default names are `"12 strips"`, then `"12 strips 2"`… Deleting a paddock
deletes its layouts.

`copy` makes a new paddock of the same shape named `"P3 copy"` (then `"P3 copy 2"`…), moved by
`offset_m` metres east and north (each within 10 km) when given. Notes, props and grazing history
stay with the original.

<!-- @F -->

## Pre-send check (op-engine, op-ingest)

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/herds/:id/check` | `CheckRequest` (hand) | `CheckResult`; 400 when the shape can't be sent, 404 for an unknown herd |

```ts
CheckRequest { geometry: Polygon, warn_m?, effective_at?, sweep?: boolean }
CheckResult  { sent: Polygon, legacy?: Polygon, facts: CheckFacts, findings: Finding[], sweep?: SweepPreview }
CheckFacts   { area_ha, head, m2_per_head, grazing_days?, forage_kg_dm?, forage_source?: "ndvi" | "measured",
               rest_days?, sweep_minutes?, vertices, holes }
SweepPreview { back_lines: LonLat[][], minutes }
Finding      { code, severity: "info" | "warning" | "critical", text, geometry?, targets?: [kind, id][] }
```

What a boundary would do, without sending it. Nothing is stored, and the check never blocks a send.

**Every herd boundary is shaped by the exclusions in effect when it takes effect** (its
`effective_at` when that is ahead, else now): farm-wide ones and those of the paddocks it touches.
An exclusion inside becomes a hole (grown to 110 m² when smaller), one across the edge is cut out,
one closer to the edge than the collars' gap is joined to it by a notch, close ones merge into one
hole, one covering the whole shape is left alone. This happens on every path (a farmer's draw, an
applied decision, each sweep step, a staged boundary, a reissue), so an exclusion that starts later
takes effect on the first send at or after it starts, and one that has ended no longer shapes
sends. An exclusion the shape already keeps out is left as it is, so a prepared boundary sent again
doesn't change. Hazards, roads, neighbour lines, water and the farm boundary are only checked.

`sent` is what sending the same shape now stores, byte for byte (holes and cuts from exclusions,
fitted to the herd's collars); a sweep toward it ends on exactly this shape. `legacy` is the single
ring (at most 64 corners, no holes) that firmware 0.1 collars enforce, when the herd has one.

Facts (SI): `area_ha` of `sent`; `head` the herd's count; `m2_per_head`; forage from the paddock
holding the shape's centre (a height measured in the last 21 days, else imagery; nothing while snow
or dormancy withholds imagery): `forage_kg_dm` above the residual over the shape and `grazing_days`
= 60 % of it at 11.8 kg DM per animal unit a day; `rest_days` since that paddock was last grazed
(0 while a herd is in it; else the latest of `grazed_until`, an applied move out of it, the
newest collar fix in it and the days collars spent in it, any herd); `vertices` and `holes` of `sent`; `sweep_minutes` when previewed.

Findings, most severe first:

| Code | Severity | When | `geometry` |
| --- | --- | --- | --- |
| `crosses_road` | critical | a road runs through it | the road inside |
| `crosses_neighbour_line` | warning | a neighbour line runs through it | the line inside |
| `crosses_farm_boundary` | warning | it goes past the farm boundary | the part outside |
| `overlaps_hazard` | warning | it takes in a hazard (a point's `radius_m`, or an area) | the overlap |
| `overlaps_exclusion` | info, warning | it overlapped an exclusion: kept out (info), or the exclusion covers all of it (warning) | the overlap |
| `no_water` | warning | the farm has water mapped and none is inside | |
| `water_inside` | info | one per water point or area inside | the water |
| `forage_short` | warning | grass for under half a day | |
| `area_per_head_low` | warning | under 25 m²/hd for cattle, 4 for sheep and goats | |
| `rested_short` | warning | its paddock rested under 21 days and the herd isn't on it | |
| `weak_coverage` | warning | 3+ 10 m cells inside with median accuracy ≥ 5 m or under 90 % of fixes, last 7 days | the cells, clipped |
| `collars_offline` | warning | herd collars (not parked) with no report for 20 min | their last fixes |
| `collars_no_holes` | warning | it has holes and some collars can't hold them | |
| `animals_in_new_holes` | warning | animals inside a hole the active boundary doesn't have | their positions |
| `animals_outside` | warning (info when a previewed sweep walks them in) | animals outside it | their positions |
| `slots_full` | warning | staged, and some collars have no free slot or slot bytes for it yet | |
| `simplified` | info | fitting to the collars changed the shape | |

Targets name what a finding is about: `["collar", id]`, `["feature", id]`, `["paddock", id]`.

**Sweep preview** (`sweep: true`): the move driver's own planner and step rule, run forward from
the herd's fresh positions (escaped and parked collars left out): each step at least 30 s after
the last, and every animal inside a step's warning band walking 1 m clear of it. `back_lines` are
where the back of the sweep will be, first to last, about every 10 m; `minutes` is the steps times
this herd's own seconds per step over its last finished sweeps (five at most, of five steps or
more), else 30 s plus half the fast poll and report intervals (40 s at the defaults). No preview
when the herd is already inside, has no fresh positions, or the sweep can't finish.

MCP `check_boundary` (read) `{ herd_id?, geometry, warn_m?, effective_at?, sweep? }` returns the
same `CheckResult`.

<!-- @S -->

## Strip schedules (op-ingest, op-engine)

A herd walked across a paddock's strips on a cadence. Each open and each back-fence step is
**staged on the collars ahead of time** (a boundary with `effective_at`), so strips open on the
collars' own clocks when the server or the network is away.

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/schedules` | `?herd_id&status=active\|paused\|done\|running` | `Schedule[]` newest first (`running` = active or paused) |
| POST | `/api/schedules` | `NewSchedule` | 201 `Schedule`; 400 bad strips/times, 409 the herd runs one already or has no boundary yet |
| POST | `/api/schedules/preview` | `NewSchedule` | `{ schedule: Schedule, moves: ScheduledMove[] }`, checked the same way, nothing stored |
| GET | `/api/schedules/:id` | | `Schedule` |
| GET | `/api/schedules/:id/moves` | | `ScheduledMove[]` in time order: history, held and skipped places, the queue |
| POST | `/api/schedules/:id/skip` | `{ index }` | `Schedule`: strip `index` (0-based) doesn't open; later strips move up one occurrence each |
| POST | `/api/schedules/:id/hold` | | `Schedule`: today's strip again; the next open and everything after move one occurrence later |
| POST | `/api/schedules/:id/move-now` | | `Schedule`: the next strip opens now (immediate); later opens keep their times |
| POST | `/api/schedules/:id/time` | `{ index, at }` | `Schedule`: that open (and its back-fence steps) at `at`; 400 when it would fall before the move before it or past the next open |
| POST | `/api/schedules/:id/pause` | | `Schedule`: nothing staged or opened until resumed |
| POST | `/api/schedules/:id/resume` | | `Schedule`; opens that passed meanwhile move on by whole occurrences |
| POST | `/api/schedules/:id/end` | | `Schedule` (`done`); 409 once ended |

Reads are any role's; every other method is a manager's.

```ts
NewSchedule { herd_id, layout_id?, paddock_id?, strips?: Polygon[], next_index?,
              cadence?: Cadence /* daily 07:00 */, starts_at? /* the next time the farm's clock reads cadence.at */,
              back_fence?: BackFence }
Cadence   { every_days /* 1-60 */, at: "HH:MM" /* farm time */ }
BackFence { enabled: true, lag_strips: 0, close_after_min: 240, close_steps: 3, close_every_min: 10 }  // defaults
Schedule  { id /* sch_… */, herd_id, paddock_id, layout_id?, strips: Polygon[], next_index /* next strip to open, 0-based */,
            cadence, starts_at, back_fence, status: "active"|"paused"|"done", created_by: Actor,
            created_at, updated_at, planned_end? /* when the last strip was planned to be done, as made */, ended_at? }
ScheduledMove { schedule_id, index /* strip */, step /* 0 open, 1.. back-fence steps */, at, geometry /* as planned, before prepare */,
                boundary_version?, skipped?: "late"|"skipped"|"held", state: "planned"|"staged"|"done"|"skipped",
                applied_at? /* the first collar's own apply time */ }
```

**Strips and shapes.** `strips` are copied (a layout re-cuts when its paddock is reshaped); with
`layout_id` and no `strips` they come from the layout, and the paddock from the layout, else the
herd's. The first strip to open (`next_index`) defaults to the one after the strip the herd's
boundary covers now, so the usual start is: send strip 1 (`POST /api/herds/:id/boundary`), then
schedule. Without a back fence, opening strip k stages `strips[0..=k]`. With one it stages
`strips[p-lag..=k]` (p = the strip opened before, so the animals keep the ground they stand on;
skipped strips in between are old ground too), then `close_steps` steps `close_after_min` after
the open and `close_every_min` apart sweep the old ground from the far side, the last being
`strips[k-lag..=k]`. Every shape goes through `prepare` when it is staged (exclusions active at
its time, fitting).

**Times.** Occurrence 0 is `starts_at`; occurrence n is `cadence.at` on the farm-local date
n × `every_days` days later, so "daily 07:00" opens at 07:00 local on both sides of a DST change
(a time the clocks skip opens just after the gap; in a repeated hour, the first).

**Staging.** Staged versions must rise with their times, so the staged moves are always a prefix
of the queue in time order. How far ahead: what the smallest `free` and `free_bytes` among the
herd's reporting collars allow (parked collars, and collars silent 20 minutes, are left out; they
are restaged when they report), never more than `limits.slots - 1`. The server works `free` out
from what each collar holds (`collar_slots`): its slots less the alive herd versions it holds that
aren't this schedule's, counting the herd's active version whether or not it holds it yet; bytes
the same way with `CollarLimits::record_bytes`. A V0 collar takes 15 moves ahead: at an open and
three back-fence steps a day, about 3.75 days. A schedule's boundaries carry its id as `decision_id`.

**Immediates.** A sweep step, a farmer's draw or any immediate herd boundary drops the staged
moves on the collars. The schedule stages again above it when the sequence settles: at the end of
the move, or 60 s after a lone boundary. A move whose time passed without taking effect is
applied at once if at most 30 minutes late, else marked `late` and never applied (with the
back-fence steps of an open that never happened); later moves go ahead. Queue edits that change
what is staged (skip, hold, edit time, pause, end) send the herd's current strip again as a new
immediate version so collars drop the staged moves, then stage the new plan. Move now sends the
strip itself. An escape's pen drops only that collar's staged slots; when it ends the collar gets
copies at the same times (see "Protocol v1 on the server").

**Decisions.** While a schedule is active the daily decision is about it: the context has
`schedule` (below). `STAY` keeps it (the next strip opens on time), `HOLD` (new action) repeats
today's strip (as `/hold`), and a `MOVE` to another paddock ends the schedule when it applies (a
farmer's draw included). `HOLD` without an active schedule fails the decision. `respond` with
`reject` on a proposed `STAY` while the herd's schedule is active holds: the decision becomes
`action: "HOLD"`, `status: "applied"`, `inputs.proposed_action: "STAY"`. The approval text reads
`Cows: strip 4 of 12 opens 07:00. Reply Y to keep, N to hold. Code 4821`. HOLD follows the herd's
autonomy like a MOVE (timer, auto).

```ts
// Decision context, while a schedule runs (null otherwise)
schedule: { id, status, paddock_id, strips /* count */, strip? /* the one the herd is on, 1-based */,
            cadence, back_fence: boolean,
            next: { strip, of, opens_at, opens /* "07:00" | "Wed 07:00" farm time */, stored?, collars? } | null,
            today?: { area_ha, forage_kg_dm?, days? },   // what today's strip holds for the herd
            rule: string }
```

**Alert** `schedule_not_stored` (warning, "Next strip not on every collar [120] min before it
opens"): the next move of an active schedule is due within `after_min` and some collar expected to
hold it doesn't (collars on duty that report, not out on an escape, five minutes after it was
staged). One alert per schedule, targets `("schedule", id)` and the collars that lack it:
`Cows: 12 of 250 collars missing strip 4 (opens 07:00). Check coverage`.

**Brief** line `schedule` (order 20): `Strip 4 of 12 opens 07:00, 248/250 stored`, or
`Schedule paused before strip 4 of 12.`

**MCP.** `get_schedule { herd_id? }` (read): `{ herd_id, schedule | null, moves, next? }`.
`schedule_strips { herd_id?, layout_id? | orientation_deg + count | width_m | days, every_days?,
at?, starts_at?, back_fence?: boolean, next_index? }` (manager): makes one; strips come from the
layout or are cut across the herd's paddock. Neither is a decision-brain tool.

**Reports.** `paddock_record` and `nrcs_528` gain "Planned days" (a schedule's `planned_end` less
the stay's start) and "Residual at exit" (the measured height nearest the day out, within two
days; `residual_cm`, else `height_cm`), each only when some row has a value.

**Moves.** A sweep now waits only on its own staged first step (a farmer's target sent for
later), not on a schedule's staged boundaries; `move_stalled`'s "waiting on a staged boundary"
reads the same way.

<!-- @A3 -->

## Texts in and the morning brief (op-alerts)

People on the farm answer and ask by text. `/api/texting*` is the owner's.

| Method | Path | Returns |
| --- | --- | --- |
| GET | `/api/texting` | `Texting` |
| PUT | `/api/texting` | merge patch over `TextingConfig` → `Texting`; 400 `poll_s` 5–300 · `approve_window_h` 1–72 · `brief.time` HH:MM. The read-only parts may be sent back and are ignored |
| PUT | `/api/texting/people/{id}` | `{ brief: bool }` → `PersonTexting`; 404 no such person |
| POST | `/hooks/twilio/sms`, `/hooks/twilio/whatsapp` | Twilio's webhook (form) → 204; 403 bad or missing signature, another Twilio account, no public URL, or texting in off |
| GET | `/v1/notify/inbox` | relay host: `?since=<cursor>&wait=<s ≤ 25>` → `Inbox`; 401 unknown key · 403 hosting off · 400 bad cursor |

```ts
TextingConfig = { inbound /* true */, poll_s /* 10 */, approve_window_h /* 12 */, brief: { enabled /* false */, time /* "06:30", farm time */ } }
Texting = TextingConfig & {
  inbound_mode: "webhook" | "polling" | "relay" | "off",   // read-only
  hooks?: { sms?, whatsapp? },                              // webhook mode: the URLs to give Twilio
  checked?: { at?, ok_at?, error? },                        // polling / relay: the last check
  people: PersonTexting[],
}
PersonTexting = { user_id, brief /* gets the brief by text */, sms_opt_out /* texted STOP */ }
Inbox = { messages: [{ id /* rin_… */, channel: "sms"|"whatsapp", from /* E.164 */, text, at }], cursor }
```

**How texts come in.** The farm's own Twilio decides: with `server.public_url` set, Twilio posts
each text to `{public_url}/hooks/twilio/sms` (or `/whatsapp`); set that URL as the number's
"A message comes in" webhook. The request must carry `X-Twilio-Signature` = base64(HMAC-SHA1(auth
token, URL + every POST parameter name and value, sorted by name)); the URL is always
`public_url` + path + query string, never the Host a proxy passes on (the default port may be
there or not). Without a public URL (a farm behind NAT) the server reads
`GET {twilio_api_base}/2010-04-01/Accounts/{sid}/Messages.json?To=<number>&DateSent>=<yesterday>`
every `poll_s` seconds instead (a Messaging Service sender is read whole, inbound only). Without
its own Twilio, a farm with the relay on long-polls the relay's inbox. Each text is taken once (by
`MessageSid`, or the relay's id) and stored in `messages` (`direction: "in"`, `kind: "inbound"`,
status `received`, or `ignored` with the reason in `error`). Texts that arrived before polling
began for a number are never acted on. Replies are queued with `kind: "reply"` on the channel the
text came in on.

**Who.** Only a verified phone of an enabled person counts; unknown numbers, unverified phones and
anyone who texted STOP get no reply (`ignored`). A 6-digit code texted back confirms a pending
verification ("Phone verified. Reply STATUS any time.").

**Texts** (case-insensitive; a command is the whole text, so "no thanks" is a question):

| Text | Needs | Does |
| --- | --- | --- |
| `Y` `YES` `SI` `SÍ` `APPROVE` / `N` `NO` `REJECT` [code \| number] | manager | answers a decision (`cycle::respond`, recorded as `{via: "text", user_id, name}`). A bare Y or N answers the decision the newest text asking this number about one was about, while it still waits and no other text in the reply window asked about another that still waits (then the numbered list); when it was answered or replaced, or nothing asked, nothing is decided and the reply names what waits now, with its code ("Cows: move to P2 was replaced. Cows: move to P3? Reply Y or N"), which the next Y answers; several waiting → a numbered list, `Y 2` picks (a number with no list texted gets the list) |
| `LATER` [code \| number] | manager | the approval prompt again in an hour (the decision's own timer is unchanged); not sent if by then the person texted STOP, was switched off, isn't a manager, or that number isn't their verified phone |
| `OK` | hand | acks the alerts the last alert text to that person covered; a decision's own text is passed over (it takes Y or N) |
| `STATUS` | anyone | each herd: head, paddock, a running move, open alerts, what waits for an answer |
| `WHERE IS <tag>`, `WHERE <tag>` [herd] | anyone | the last fix in words, its age and `https://maps.google.com/?q=<lat>,<lon>`. A tag used in more than one herd gets one line per herd (no link) and "Add the herd for a map link, like WHERE 105 Cows."; `WHERE 105 Cows` or `WHERE 105 in Cows` picks |
| `STOP MOVE` [code \| number] | manager | stops the running move where it is |
| `STOP` / `START` | anyone | opts out / in (Twilio's words mirrored: STOPALL, UNSUBSCRIBE, CANCEL, END, QUIT, REVOKE, OPTOUT; UNSTOP, and YES for a number that opted out). No reply on SMS, where Twilio confirms |
| `HELP`, `INFO` | | Twilio answers; nothing from us |
| anything else | anyone | the farm's brain answers with read tools (never `run_sql`), ≤ 320 characters on SMS, 1,000 on WhatsApp; a brain that doesn't answer questions → the list of texts. One question per person at a time and 20 an hour, 2 at once on the farm (the rest wait their turn); a question over a limit isn't asked, the first one over it is told why ("One question at a time. …", "That's 20 questions this hour. …"), the rest are logged `ignored` |

A Y, N or STOP MOVE without the decision's 4-digit code counts only within `approve_window_h` of
our last alert or brief to that number (replies don't count, so texting us can't open the window);
after that the code from the approval text is needed ("Y 4821"). Five wrong codes in an hour from
one number and codes from it stop counting for the hour. A code names the decision whose newest
`decision_waiting` alert carries it, also after a hand resolved that alert while the decision still
waits. On a relay host, other farms' texts (idempotency keys `relay:…`) never open its own farm's
window or ask about its decisions.

**The morning brief.** At `brief.time` farm time (while `brief.enabled`), everyone with the brief on
gets each of their herds' brief (`GET /api/brief`'s `text`, ≤ 480 characters) on every way their
alerts reach them. Once a farm day; a server that was down then sends it within two hours, not
later. A brief opens the reply window like an alert, and a bare Y or N to a brief that asks
("Reply Y or N.") answers the decision it asked about.

**The relay's inbox** (host side). A text to the relay's number reaches only farms whose key has
that number verified, and of those the one it answers: a Y, N, LATER or STOP MOVE with a decision's
code goes to the farm whose text carried "Code 4821"; a bare one goes to the one farm that asked
the person about a decision (`prompt: true`) in the last 12 h, and when more than one did, the host
answers itself: "More than one farm asked you. Add the code from the text you mean, like Y 4821.";
anything else goes to the farm whose text to that number was the host's last. The host's own farm
counts as one of them when it has a verified person with that phone; a key that only asked to
verify the number, or was deleted, never does. STOP and START go to every key that has the
number (Twilio opts a phone out per sender number, so STOP to the shared number stops every farm on
it); a 6-digit code from a recipient being verified is tried against every key's pending code for
that number and verifies, and goes on to, the key whose code it is (a wrong one uses a try of each). The
long-poll waits up to `wait` seconds for a text; rows up to the `since` a farm sends back are
delivered and deleted. **Dead-man**: a key that polled before and has been quiet for more than
`notify.hosting.deadman_after_min` texts its `deadman` recipients once per outage ("openpasture:
Test farm hasn't checked in for 16 min. Its power or internet may be down.").

<!-- @H -->

## Welfare record and training mode (op-analytics, H)

The collars are audio only. Cue kinds are `warn` (the warning tone in the zone inside the line,
level 1–4) and `outside` (a tone for up to 10 s after a crossing); there are no others.

| Method | Path | Query or body | Returns |
| --- | --- | --- | --- |
| GET | `/api/welfare/animals` | `herd_id?` (every herd when left out) | `HerdWelfare` |
| GET | `/api/welfare/animals/{animal_id}/cues` | `from&to` (default the last 14 days) | `AnimalWelfare` |
| GET | `/api/welfare/cues/points` | `herd_id?&from&to` (default the last 7 days) | `CuePoints` |
| GET | `/api/welfare/training/{herd_id}` | | `HerdTraining` |
| PUT | `/api/welfare/training/{herd_id}` | `{ enabled?, warn_m?, trained_after? }` (manager; left out keeps) | `HerdTraining` |

```ts
Learning       { status?: "trained"|"learning", since?, streak, // turned back in a row since the last crossing
                 outcomes: { turned_back, crossed, rest, boundary_changed },
                 last_episode_at?, derived?: true }             // some episodes rebuilt from fixes
HerdWelfare    { herd_id?, training?: Training, head /* animals not removed */, trained, learning,
                 animals: (Learning & { animal_id, tag, herd_id, collar_id? })[] }   // in tag order
AnimalWelfare  { animal_id, tag, herd_id, collar_id?, from, to, trained_after,
                 learning: Learning,                            // from all its episodes, now
                 days: { date /* farm day */, warn, outside, tone_s, episodes, longest_s?, max_level? }[],  // every farm day in the range
                 episodes: Episode[],                            // in the range, newest first
                 cues: LedgerRow[], truncated?,                  // newest first, at most 2,000
                 fit_checks: { collar_id, checked_at, by?: Actor, notes? }[],  // of every collar it wore
                 drop_offs: { from, to? }[] }                    // drop_off alerts on it or its collars, merged
Episode        { id /* epi_… */, collar_id, herd_id?, animal_id?, start, end, ring /* 0 edge, 1.. holes */, cues,
                 max_level, min_margin_m, outcome: "turned_back"|"crossed"|"rest"|"boundary_changed",
                 boundary_version?, derived?: true }
LedgerRow      { at, kind: "warn"|"outside", level, tone_ms, dur_ms? /* as the collar reported */, margin_m,
                 ring?, boundary_version?, collar_id, outcome?, episode_id?, derived?: true }
CuePoints      { cell_m: 2, size?: [dlon, dlat], ticks: [lon, lat, warn, outside][], truncated? }  // most cued first, at most 5,000
Training       { enabled: false, warn_m: 10, trained_after: 5 }  // setting `welfare.training`, per herd
HerdTraining   Training & { herd_id }
```

- **Episodes**: a run of warning tones ending `turned_back` (back inside), `crossed`, `rest` (after
  20 s of warning the collar is quiet for 30 s) or `boundary_changed`. Firmware 0.2 collars report
  them. For firmware 0.1 collars (cues with no kind) a background task, every 30 s, rebuilds them
  from the stored fixes (the server fence's state and margin at each fix) and cues by the collar's
  own rules, and stores them in `episodes` with `derived = 1`; an episode whose end hasn't arrived
  waits for the next run, and one with nothing stored after it for 10 minutes is left out. Tone
  for those collars counts 0.3 s a cue (their beep; they don't report it).
- **Learning status**: trained after `trained_after` turned-back episodes in a row with no crossing
  (rest and boundary changes neither count nor break the run); learning once it has any episode;
  no status without episodes. `since`: the episode that made it trained; for learning, the crossing
  that ended being trained, else its first episode. `trained_after` is its herd's.
- **Training mode** per herd: while `enabled`, a boundary sent for the herd without `warn_m` takes
  the training `warn_m` (op-ingest's `default_margins`); a send that names `warn_m` keeps it.
  Boundaries already sent keep theirs. `warn_m` 1–100 m, `trained_after` 1–50.
- Days are farm days (the farm's time zone). Cues are read from SQLite and the Parquet day files
  (a row in both while a rollup deletes counts once).
- MCP (read, viewers): `get_welfare` `{ herd_id?, animal_id?, from?, to? }`: without `animal_id`
  a `HerdWelfare`, with it an `AnimalWelfare` with at most 50 cues and episodes.
- Report `welfare` ("Welfare record", `GET /api/reports/welfare?from&to&herd_id`): per animal,
  cues by kind, tone (total, a day, most in one farm day), longest episode, loudest level;
  episodes by outcome, status and since as of the end of the dates, "From fixes"; fit checks and
  times the collar lay still; the farm days with cues; method notes; an Operator signature line.

Coverage (G's `/api/coverage`) gains two metrics from the collars' health reports, each report
counted in the cell of that collar's fix nearest in time (within 6 h):
- `fix_rate`: fixes the receivers got ÷ fixes they tried (`health.fix_ok / fix_attempts`), 0–1,
  unit `ratio`; `n` is the attempts.
- `cell`: the median LTE cell signal (`health.rsrp_dbm`, whole dBm), unit `dBm`; `n` is the reports
  that measured it. Only boards with a modem report one.

Alert rules (H): `drop_off` also reads a collar's IMU (`health.still_s`, `tilt_deg`): still for
45 min (or the rule's `after_min` if sooner) and tilted past 60° is a collar lying on its side
(`data.imu: true`, `tilt_deg`); an IMU collar is judged on that alone. `fit_check_due` (info, no
texts): a collar on an animal whose fit hasn't been checked for `fleet.fit_check_days` since its
last check (else since it was added); the brief counts them ("Fit check due: 5").
<!-- @L -->
<!-- @M -->
<!-- @Z -->
<!-- @X1 -->

## Seams between streams (X1)

**Live feed, one message per farm window.** Every 500 ms window gathers the whole farm's
batches. A window holding only one of them sends it as itself (`positions`, `ack_batch` or
`cue_batch`, as above); a window holding more sends one message:

```ts
{ type: "batch", events: Event[] }   // each herd's positions, ack_batch, cue_batch, in herd id order
```

So several herds live at once still make at most two batched messages a second for the farm
(before: up to three per herd). Clients read a `batch` as its events in order. Every batched
event is for any role, so viewers get the same message; `message` events never ride in a batch.

**Reject codes on the feed.** A collar's `rejected` ack with a protocol v1 code
(`hole_too_close`, `slots_full`, …) carries it on the bus (`ack.code`) and in `AckItem.code`.

**Someone else's alert prefs.** `PUT /api/alerts/prefs/{id}` is the owner's at the guard (people
ids are `usr_…`), so a manager is refused before the body is read; `PUT /api/alerts/prefs/me`
stays a hand's.

**The brief after a stopped move.** When today's decision is a MOVE (or HOLD) whose move someone
stopped before the target, where it stands reads `Stopped 610 ft short, 250/250 collars
confirmed.` (the distance through the farm's units), also the next morning.

**Imported history and collar data.** Committing a position import leaves out points the
animal's collar already recorded, so an animal-hour's dwell is counted once, from the collar.
`POST /api/import/positions/{id}/commit` gains:

```ts
{ …, collar_covered: number }   // points left out because the animal's collar recorded that time
```

"Recorded" is read at the dwell rule's resolution: for hot fixes, the 30-minute buckets
holding a fix of that collar and animal; for days already rolled to Parquet, the stretch the
collar day's dwell covers, ending 30 minutes after its last fix, credited to the animal the
collar is on now. A file with nothing new left is 409.

**Paddock areas stored before op-geo measured rings whichever way they wind** (a clockwise
outer ring, or a hole wound like its outer ring) are measured again from their geometry when
the data dir opens, in `paddocks` and in the geometry history reports use. Rows that are
right are left alone.

**Rest days at 250 collars.** "Last grazed" for signals, the decision context and
`GET /api/layers/paddocks` reads the newest fix per herd and paddock from `fix_paddock_last`
(kept by a trigger as fixes land), not the herd's hot fixes; the herd's position over the last
day reads at most 20,000 fixes, sampled per collar and time bucket by index seeks when the day
holds more. No response shape changes.
