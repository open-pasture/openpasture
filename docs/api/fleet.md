# Fleet API: units, factory tests, provisioning, firmware, herd network

For anyone building a test station, a provisioning tool, a bridge or their own collars against
an openpasture server. The routes live in `crates/op-fleet`. Conventions (ids, errors, auth,
time format) are those of [`docs/API.md`](../API.md): JSON bodies, RFC 3339 UTC times,
`{ "error": "message" }` on failure.

A **unit** is one piece of hardware, known by its serial from the moment a station registers it.
A unit becomes a **collar** (the farm record in `/api/collars`) when a farm claims it. How to run
a station: [`MANUFACTURING.md`](../MANUFACTURING.md).

Identity follows the OpenCollar protocol v2 draft, `opencollar/protocol/v2/identity.md`:
serials, device ids, claim codes, the card and the factory record are the ones defined there.
Firmware follows `firmware.md` and `server.md` §8. Byte strings in JSON are lowercase hex.

## Contract

These shapes are fixed. Fields may be added; none of these will be removed or change meaning.

### Who may call

| Caller | How | May |
| --- | --- | --- |
| Owner, manager | app token, a person's token, or a local request | everything below; owner-only routes are marked **owner** |
| Station | `Authorization: Bearer ops_…` (a station token, see [Stations](#stations)) | only `POST /api/fleet/units`, `GET /api/fleet/test-plans/:device_class`, `POST /api/fleet/units/:serial/tests`, `POST /api/fleet/units/:serial/provision` and `GET /api/fleet/units/:serial/label.svg`. Anything else is 403; a revoked or unknown token is 401 |
| Hand, viewer | | nothing in this API (403) |

`GET /api/fleet`, `/api/fleet/settings` and the fit-check routes are fleet care (op-analytics,
`docs/API.md` "Coverage and fleet care"), not this API.

### Register a unit

`POST /api/fleet/units`

```json
{
  "serial": "OCN-2611-00123",
  "device_class": "node",
  "hw_rev": "N1.0",
  "fw_version": "1.0.0",
  "device_pubkey": "base64 (or base64url) of the unit's 32-byte Ed25519 public key",
  "radios": { "lora_eui": "AC1F09FFFE0A1B2C", "ble_addr": "D4:12:AB:00:01:FF" },
  "station": "bench-1",
  "image_keys": ["9f3c2a71b0d4e655"],
  "caps": ["holes", "slots", "firmware"],
  "claim_code": "7Q4K9M2XH8D3TV5W",
  "factory_record": "d28443a10127…",
  "force": false
}
```

- `serial`: 1–32 characters of `A–Z 0–9 -` (OpenCollar's form: `OC` + class letter + `-` + year
  and week + `-` + a 5-digit counter). It is the unit's id everywhere below.
- `device_class`: `node`, `anchor`, `station`, `direct` (the protocol's classes), or any other
  class a builder names (`trough`, `gate`…): lowercase letters, digits, `-` and `_`, 1–32.
- `hw_rev` 1–16 characters, `fw_version` 1–32.
- `device_pubkey` is generated on the unit at manufacture and never leaves it in private form.
- Optional: `radios` (`lora_eui`, `imei`, `iccid`, `ble_addr`); `station` (a station token's own
  name is used when absent); `image_keys`, the kids of the firmware keys the unit's bootloader
  trusts; `caps` fixed by the hardware; `claim_code`, when the fixture drew it and wrote it to the
  unit (16 Crockford base32 characters; `-`, spaces and lower case are fine); `factory_record`,
  the fixture's maker-signed record (hex COSE_Sign1). A record must name the same serial, key,
  class and board, and its claim check must fit the claim code; one from a maker key this server
  doesn't know is kept and marked unverified.
- **Idempotent on serial.** Registering the same serial again with the same key returns the unit
  (200) and updates the board, firmware, radios, station, keys, caps and claim code. A different key
  for an existing serial is refused (409) unless `force: true` and the caller is the owner (the
  old key stays in the unit's history; the unit goes back to `built`). A key already registered to
  another serial is refused (409).
- 201 on first registration, 200 after.

Returns a `Unit`:

```ts
Unit {
  serial, device_class, hw_rev, fw_version,
  device_pubkey, device_kid, device_id,       // device_id = device_kid: first 8 bytes of SHA-256 of the key, hex
  radios: { lora_eui?, imei?, iccid?, ble_addr? },
  image_keys?: string[], caps?: string[],
  state: "built"|"tested"|"provisioned"|"claimed"|"in_service"|"retired"|"rma",
  generation, generation_at?,                 // up by one on RMA, retire or a new key (server time it began)
  origin: "station"|"card",                   // registered here, or claimed from another builder's card
  station?,                                   // last station that registered or tested it
  claim_code?,                                // 16 characters, until the unit is in service
  claim_checked: boolean,                     // drawn or written here, or matched a factory record's claim check
  collar_id?, herd_id?,                       // once claimed
  last_test?: { id, result: "pass"|"fail", plan_id, plan_version, finished_at, seq },   // the run that decides (below)
  maker?, maker_kid?, maker_verified: boolean, made_at?,   // from its factory record
  provisioned_at?, claimed_at?, created_at, updated_at,
}
```

Lifecycle: `built` → `tested` (a passing test run) → `provisioned` → `claimed` → `in_service`
(the collar's first report) → `retired` or `rma`. A failing run sends `tested` or `provisioned`
back to `built`. An `rma` unit goes back to `tested` on a fresh passing run, and provisioning it
again returns it to its collar. A `retired` unit comes back only through `rma`.

**Which run decides.** `last_test` is the latest run by the server's own order (`seq`, given in
the write that stores the run, never a station's clock) among the **counted** runs of the unit's
current **generation**. RMA, retiring and a new device key start a new generation, so no earlier
pass counts: a repaired unit needs a fresh pass. Every transition (test, provision, claim, state)
reads the unit in the same write that changes it and changes it only if its state and generation
are still the ones it read, so a retire or a failure that lands meanwhile wins (409 "changed
meanwhile" for the loser).

### Test plans

`GET /api/fleet/test-plans/:device_class` → the current plan for that class:

```ts
TestPlan {
  id, version, device_class,
  steps: [{ name, kind: "measure"|"check"|"record", limit_lo?, limit_hi?, unit?, required, note? }],
  shipped: boolean,                           // true for the defaults this server ships
  created_at,
}
```

- `measure`: the station sends `value`; the server passes it when `limit_lo ≤ value ≤ limit_hi`
  (either limit may be absent). `check`: the station's `status` decides. `record`: the value is
  kept (an IMEI, a firmware hash) and never fails.
- `required: false` steps are recorded and shown but don't fail the run.
- Shipped defaults exist for `node` and `anchor`; other classes are 404 until the owner adds a plan.
- Plans are versioned. Editing makes a new version; runs keep the version they were tested to.

### Post a test run

`POST /api/fleet/units/:serial/tests`

```json
{
  "plan_id": "node-fct", "plan_version": 1,
  "station": "bench-1", "fixture_id": "fx-01",
  "started_at": "2026-10-26T14:02:11Z", "finished_at": "2026-10-26T14:03:05Z",
  "run_key": "bench-1/OCN-2611-00123/1793023331000", "generation": 0,
  "steps": [
    { "name": "rail_3v3", "status": "pass", "value": 3.31, "unit": "V" },
    { "name": "sleep_current", "status": "pass", "value": 4.2, "unit": "uA" },
    { "name": "gnss_cn0", "status": "fail", "value": 22, "unit": "dBHz", "detail": "re-radiator off?" }
  ]
}
```

`run_key` (optional, ≤ 64): the station's own id for the run; sending it again returns the stored
run (200) instead of a new one, and a different run with the same key is refused (409).
`generation` (optional): the unit's generation when the station registered it; if the unit was
sent back or retired since, the run is refused (409, test again).

Returns a `TestRun` (201) with the **server's** verdict against the plan's limits:

```ts
TestRun {
  id, serial, plan_id, plan_version, station?, fixture_id?, started_at, finished_at, received_at,
  result: "pass"|"fail",
  seq, generation, counted: boolean, run_key?,
  failed: string[],                           // required steps that didn't pass, in plan order
  steps: [{
    name, status?, value?, unit?, detail?,               // as sent
    verdict: "pass"|"fail"|"skip"|"missing"|"extra",
    why?,                                                // e.g. "2.1 V below 3.2 V"
    kind?, limit_lo?, limit_hi?, plan_unit?, required,   // from the plan
  }],
}
```

- A required step passes only when it is present, not `skip`, its status isn't `fail`, its value
  is within the limits (`measure`), and its unit matches the plan's when both are given. The
  server's verdict wins over the station's `status` for `measure` steps.
- A step the plan doesn't have is kept as `extra` and doesn't count.
- `value` is a number, `true`/`false` or a short text. Times must be in order, under a day, and
  not in the future (a station with a wrong clock is told so).
- The unit must be registered (404) and not `claimed`, `in_service` or `retired` (409: mark it
  `rma` first). The plan version must exist for the unit's class (400).
- `counted: false`: a **pass** that finished before a run of this unit already stored (a
  disconnected station delivering late), or before the unit's generation began. It is kept and
  shown, but can't make the unit eligible. A failure always counts.

### Provision

`POST /api/fleet/units/:serial/provision`, body optional:
`{ "claim_code": "<the fixture's>", "rotate_claim": false }`.

Refused (409) unless the run that decides (above) passed (or the unit came from another builder's
card and is claimed here), never for `built`, `rma` or `retired` units, and, for units that talk
LTE, until the server has a collar address.

```ts
Provisioning {
  serial, device_id,
  server_key, server_kid,                     // this server's Ed25519 public key (base64) and its kid
  endpoint?,                                  // https://…/collar/v1, once a collar address is set
  link: { transport: "lora"|"lte"|"lora+lte", lora_region, report_s, poll_s },
  claim_code,                                 // 16 characters; kept across re-provisioning unless rotated
  card,                                       // the card's QR text: OC2:<serial>:<device key, base64url>:<claim code>
  factory_record?: { cose, record, maker_kid, verified },   // hex COSE_Sign1 and its JSON view
}
```

- The claim code is the fixture's when it sends one (here or at registration), else drawn here.
- The factory record is the fixture's own when it sent one and the claim code hasn't changed;
  otherwise this server signs one with its **maker key** (`<data>/maker_ed25519.key`), naming the
  maker set under [Provisioning settings](#provisioning-settings), the latest passing test
  (fixture, passed, SHA-256 of the judged steps, time) and the claim check. Classes the protocol
  doesn't name get no record.
- The unit becomes `provisioned` (`claimed` again if it already belongs to a collar).
- Getting the server key onto a v2 collar is the **bind** command at claim time (it needs the
  collar's claim nonce from its hello); the collar ingest sends it.

### Claim

A farm takes a unit into a herd. Both routes create a `Collar` (`/api/collars`) in the herd and
link it to the unit.

`POST /api/fleet/units/:serial/claim` `{ claim_code, herd_id, name?, animal_id? }`

`POST /api/fleet/claim` `{ claim_code? | card?, factory_record?, herd_id, name?, animal_id? }`: by
claim code alone, or by the scanned card. A server that never saw the unit registers it from the
card (`origin: "card"`; class, board and firmware from `factory_record` when given, else
`unknown`).

- **The typo check** (`identity.md` §3, key 8): whenever there is a factory record (sent with the
  card, or stored here), the card's code must match its `claim_check` before anything is stored;
  a mismatch is a wrong code (404). A card with no record is taken at its word: `code_checked` is
  false and the collar checks the code when the bind reaches it. Either way the unit stays
  `claimed` (not `in_service`) until the collar reports.
- **Reclaim after a reset**: a unit in service here has forgotten its code. Its card, whose code
  matches the stored factory record's claim check, claims it again: the same collar (history,
  herd, animal), a new collar key (the old one stops working), fence state starting over, the
  unit `claimed` until it reports under the new key. Returns **200** with `reclaimed: true`. The
  body's `herd_id` must be the collar's herd and `animal_id`, if given, its animal (409
  otherwise: move it from the collar's page afterwards). This needs the stored record, so units
  this server didn't record a claim check for can't be reclaimed this way.

Returns 201:

```ts
Claimed {
  unit: Unit,
  collar: Collar,                             // named after the serial unless `name` is given
  v1: { key, public_key, endpoint?, line?, why? },   // firmware that speaks protocol v1: shown once
  code_checked: boolean,                      // see the typo check
  reclaimed?: true,                           // a reset unit claimed again (200)
}
```

- Wrong or unknown claim code: 404 (no hint which part was wrong). Five wrong codes a minute per
  caller: 429.
- The unit must be `provisioned` (409 otherwise; a claimed one names the herd it is in).
- `v1.line` is `provision {"v":1,…}` for a v1 collar's console; without a collar address it is
  absent and `why` says so.
- The unit turns `in_service` on the collar's first report, and its claim code is forgotten then
  (`server.md` §10: claim codes only as long as a claim takes).

### Live events

On `/api/live`, managers and up: `{"type":"fleet_unit","unit":Unit}` on every registration, test,
provisioning, claim and state change, and `{"type":"fleet_test","run":TestRun}` for each run.

## Units

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/fleet/units` | `?state=&device_class=&station=&q=&limit=&cursor=` (`q` matches serial, device id, a radio id or a collar id) | `{ units: Unit[], total, next? }`: newest first, `limit` per page (default 200, max 1000), `total` matching the filters; pass `next` as `cursor` for the next page (keyset: units registered meanwhile don't shift pages) |
| GET | `/api/fleet/units/:serial` | | `UnitDetail { unit, factory_record?, runs: TestRun[], events: UnitEvent[], keys: [{ device_pubkey, device_kid, from, to? }] }` |
| POST | `/api/fleet/units/:serial/state` | `{ state: "retired"\|"rma", note? }` | `Unit` |
| GET | `/api/fleet/units/:serial/label.svg` | | the label (SVG, 50 × 25 mm), 409 before it has a claim code |
| GET | `/api/fleet/summary` | | `{ total, by_state: {state: n}, by_class: {class: n} }` |

`UnitEvent { at, kind, detail, by? }`; kinds: `registered`, `key_replaced`, `tested`,
`provisioned`, `claimed`, `reclaimed`, `in_service`, `state`, `firmware`.

## Batch views

| Method | Path | Query | Returns |
| --- | --- | --- | --- |
| GET | `/api/fleet/yield` | `plan_id` (required), `plan_version?`, `from?`, `to?`, `station?` | `Yield` |
| GET | `/api/fleet/throughput` | `from?`, `to?` (default the last 7 days), `bucket: "hour"\|"day"` | `Throughput` |

```ts
Yield {
  plan_id, plan_version, runs, units,         // one version only: the one asked for, else the latest
  first_pass: { units, passed, rate },        // each unit's first run
  final: { units, passed, rate },             // each unit's latest run
  steps: [{ name, required, runs, pass, fail, skip, missing, rate,
            values?: { n, min, p50, max, mean, unit? } }],   // in plan order
  failures: [{ name, fail }],                 // required steps by failures, most first
  other_versions: [{ plan_version, runs }],   // runs in the window judged by other versions: never in the numbers
}
Throughput {
  from, to, bucket,
  stations: [{ station, runs, units, pass, fail, cycle_s_p50?,
               buckets: [{ at, runs, pass, fail }] }],
}
```

## Test plans

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/fleet/test-plans` | | the current `TestPlan` of every class |
| GET | `/api/fleet/test-plans/:device_class` | `?version=` for an older one | `TestPlan` |
| PUT | `/api/fleet/test-plans/:device_class` | **owner** `{ steps }` | a new version, `TestPlan` (201) |

Step names: lowercase letters, digits and `_`, unique within a plan, 1–64 steps. Only measure
steps have limits, and they need at least one.

Shipped defaults (`node-fct` v1, `anchor-fct` v1), from the end-of-line test in
`opencollar/docs/LORA-V1-PLAN.md` and `TEST-FIXTURE.md`: `fw_flash`, `rail_vsys`, `rail_3v3`,
`sleep_current`, `charger`, `flash_ext`, `imu`, `hall_wake`, `buzzer_left`, `buzzer_right`,
`gnss_fix`, `gnss_ttff`, `gnss_cn0`, `lora_tx_rssi`, `lora_rx_rssi`, `ble`; the anchor adds
`fuel_gauge`, `lte_attach`, `lte_rsrp` and `sim_iccid`. Limits are starting points for a station
like the one in `TEST-FIXTURE.md`; RSSI limits depend on the fixture's reference radio and
distance, so set them for your station.

## Stations

A station is the PC beside the fixture. Its token can only do the five station routes.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/fleet/stations` | **owner** | `Station[]` |
| POST | `/api/fleet/stations` | **owner** `{ name }` | 201 `{ station: Station, token }`; the token is shown once |
| POST | `/api/fleet/stations/:id/revoke` | **owner** | `Station`; its token stops working at once |

`Station { id: "stn_…", name, created_at, last_used_at?, revoked_at? }`

## Provisioning settings

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET PUT | `/api/fleet/provisioning` | PUT **owner** `{ lora_region, maker? }` | `{ lora_region, maker?, maker_kid, maker_key }` |

`lora_region`: `US915` (default), `AU915`, `EU868`, `AS923`, `IN865`, `KR920`, `CN470`. `maker`:
the name in the factory records this server signs (the farm's name when unset). `maker_kid` and
`maker_key` (base64) are this server's maker key; give the key to the farms you ship to, so they
can enrol it (`POST /api/fleet/signing-keys` with `purpose: "maker"`).

## Labels

`GET /api/fleet/units/:serial/label.svg` draws the unit's label: serial, class and board, the
claim code on two lines of two groups, and a QR code (error correction M) holding the card text
`OC2:<serial>:<device key, base64url>:<claim code>`. The app prints labels for one unit or a
selection at `#/print/fleet-labels/<serial>,<serial>…`.

Every text line fits the 24 mm column right of the QR at the longest values (32-character
serial, which wraps to two lines at a `-`; 32-character class plus 16-character board): sizes
are worked out from a monospace advance of 0.602 em and each line carries that `textLength`, so
a printer's own monospace font can't push it past the edge. The claim code prints at 3.2 mm, the
serial at 3.4 mm (no smaller than 2.1 mm when it wraps). Print and scan one at real size before a
batch: no physical printer or scanner was tested.

## Firmware

Updates are automatic by default (`firmware.md`): a release reaches every collar it fits in
stages; nothing is installed while the herd is being moved or the collar is out on an escape, or
below the battery floor; the collar rolls itself back when a new image doesn't confirm.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/fleet/firmware` | | `Firmware[]`, newest first |
| POST | `/api/fleet/firmware` | **owner** `{ manifest, channel?, notes? }` | 201 `Firmware` |
| PUT | `/api/fleet/firmware/:id/image` | **owner**, the image bytes (`application/octet-stream`, ≤ 4 MB) | `Firmware` (`has_image: true`); 400 when the size or SHA-256 doesn't match the manifest |
| POST | `/api/fleet/firmware/:id/withdraw` | **owner** | `Firmware`; its rollouts stop |
| GET PUT | `/api/fleet/firmware/policy` | `FirmwarePolicy` | `FirmwarePolicy` |
| GET | `/api/fleet/firmware/rollouts` | | `Rollout[]` |
| POST | `/api/fleet/firmware/rollouts` | `{ firmware_id }` | 201 `Rollout` (by hand: a beta release, or while the policy is paused) |
| POST | `/api/fleet/firmware/rollouts/:id/pause` / `resume` / `cancel` | | `Rollout` |
| GET | `/api/fleet/firmware/units` | `?herd_id=&rollout_id=` | `UnitFirmware[]` |
| GET | `/api/fleet/signing-keys` | | `SigningKey[]` |
| POST | `/api/fleet/signing-keys` | **owner** `{ name, public_key, purpose?: "image"\|"maker" }` | 201 `SigningKey` |
| POST | `/api/fleet/signing-keys/:kid/revoke` | **owner** | `SigningKey` |

```ts
// manifest: the release manifest (protocol v2 type 65), a COSE_Sign1 by a release or image key, as hex.
Firmware {
  id: "fwi_…", version, classes: string[], hw_revs: string[], sha256, size, signer_kid,
  security_counter, min_battery? /* % */, revoke?: string[], channel: "stable"|"beta", notes?,
  manifest /* hex */, has_image, created_at, withdrawn_at?, signer?: { kid, name, kind: "vendor"|"owner" },
}
FirmwarePolicy {
  mode: "auto"|"paused",                      // auto by default
  channel: "stable"|"beta",
  battery_floor,                              // 0–1, default 0.4 and never below the collar's own 40 %
  wave_hours,                                 // default 24
  held_herds?: string[],                      // herds held at the version they run
}
Rollout {
  id: "rol_…", firmware_id, version, classes,
  state: "active"|"paused"|"done"|"cancelled", paused_reason?,
  waves: [{ kind: "collar"|"herd"|"all", after_h, started_at?, collar_id?, herd_id? }], wave,
  counts: { targeted, waiting, held, ready, offered, confirmed, failed, rolled_back, unsupported },
  created_at, created_by?,                    // "auto" for automatic rollouts
}
UnitFirmware {
  serial?, collar_id, name, herd_id?, current?, target?, rollout_id?,
  state: "current"|"waiting"|"held"|"unsupported"|"ready"|"offered"|"confirmed"|"failed"|"rolled_back",
  reason?,                                    // "The herd is being moved", "Battery 31 %, needs 40 %",
                                              // "This collar can't take updates over the air yet", …
  updated_at,
}
```

- The manifest must verify against a firmware key this server trusts: the OpenCollar release
  keys built into the server, or an image key the owner enrolled (for builders who sign their own
  firmware). Kids a verified manifest lists in `revoke` stop being trusted here; a revoked key's
  releases can't be registered and its rollouts stop.
- A release fits a collar claimed from a unit whose class it lists, whose board it names exactly,
  and whose bootloader trusts its signer (when the factory record said which keys).
- **Stages** (`server.md` §8): one collar of the herd with the most collars it fits; the rest of
  that herd once that collar confirmed and `wave_hours` have passed; every herd at three times
  `wave_hours`. A stage never starts while a collar of an earlier one is mid-install. A
  `rolled_back`, a failure, or a collar not heard for 2 h after it wrote the image pauses the
  rollout with the reason.
- A collar is offered a release only when it reports the `firmware` capability (protocol
  registry). Until then its state is `unsupported`: *This collar can't take updates over the air
  yet.*
- Delivery is the protocol's firmware command (type 9), which the collar ingest builds from
  `op_fleet::firmware::offer_for`; images are served from `op_fleet::firmware::image_bytes`.
- **An offer is a reservation.** `offer_for` reads every gate (rollout active, policy, held herd,
  a move, an escape, the battery) in the same write that marks the collar `offered` with a new
  `offer_id`, so a pause or a move that commits first wins. `still_good(ctx, collar_id,
  offer_id)` reads them again right before the bytes go out and takes the offer back (`held`,
  with the reason) if anything changed. A pause acknowledged after `still_good` returned can't
  stop bytes already handed to the radio.
- **Lost offers.** No answer at all (not even "written") within 30 min: the offer is dead and the
  collar is offered again after 15 min, doubling up to 6 h; after 5 offers it has `failed`
  ("No answer to 5 offers"), which pauses the rollout.

## Herd network

Per herd: which collars are anchors (LoRa + LTE, the herd's link) and which are nodes (LoRa
only), which anchor is active, and how well each node is heard. Collars that talk to the server
themselves (protocol v1 over LTE) are `direct`.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/fleet/network/:herd_id` | | `HerdNetwork` |
| GET POST | `/api/fleet/anchors` | `{ name, point: [lon, lat], serial?, herd_ids? }` | `FixedAnchor[]` / 201 `FixedAnchor` |
| PATCH DELETE | `/api/fleet/anchors/:id` | partial | `FixedAnchor` / 204 |

```ts
HerdNetwork {
  herd_id, active_anchor?: string,            // collar id or fixed anchor id
  anchors: [{ id, name, source: "collar"|"fixed", active, last_beacon_at?, battery?, lte_rsrp_dbm?, serial? }],
  members: [{ collar_id, name, role: "anchor"|"node"|"direct"|"unknown", serial?,
              last_heard_at?, via?, rssi_dbm?, snr_db?, quality: "good"|"fair"|"poor"|"lost"|"unknown" }],
  gaps: [{ collar_id, name, since, point?: [lon, lat] }],   // nodes not heard for 3 report intervals (at least 15 min)
}
FixedAnchor { id: "anc_…", name, point: [lon, lat], serial?, herd_ids: string[] /* [] = every herd */, created_at }
```

Roles come from what the network reports (v2 membership), else the unit's device class
(`anchor` and `station` are anchors), else `direct` for a collar that reports by itself. Quality
is from the last uplink: good at RSSI ≥ −110 dBm and SNR ≥ 0 dB, poor below SNR −7.5 dB or RSSI
−120 dBm, fair between; `lost` once unheard for three report intervals. Until a herd has v2
reports, members show `quality: "unknown"`.

## Collar console

`GET /api/fleet/collars/:id/console` → everything the per-collar console shows, in one read:

```ts
CollarConsole {
  collar: Collar, unit?: Unit,
  contact: { state: "waiting"|"heard", since, endpoint?, why?, last_seen?, latency_s? },
  health?: { at, battery?, battery_v?, charging?, temp_c?, uptime_s?, reset?, still_s?, tilt_deg?,
             fix_attempts?, fix_ok?, cell?: { rsrp_dbm?, rsrq_db?, snr_db?, mode?, band?, cell_id?, tac? } },
  fix?: { at, point, accuracy_m, sats, cn0?, hdop?, ttf_s? },
  acks: [{ at, received_at, version, status, code?, reason?, command_id }],   // latest 50
  cues: [{ at, kind, level, margin_m, ring?, dur_ms?, boundary_version? }],   // latest 50, with or without an animal
  firmware?: UnitFirmware,
  network?: HerdNetwork["members"][number],
}
```

`contact.state` is `waiting` until the collar's first report; `latency_s` is the last contact
(server time) minus the newest fix (collar time): clock drift or a store-and-forward backlog.
Slots, config and limits come from `GET /api/collars/:id/slots`; the wire log from
`GET /api/collars/:id/wire` (op-ingest).

## Rust hooks for collar ingest

For the code that reads collar reports (op-ingest, the v2 ingest); each is cheap and safe to call
on every report, and op-fleet depends on op-core alone so there is no cycle:

- `op_fleet::network::record_uplink(ctx, &Uplink { collar_id, role, via, rssi_dbm, snr_db, at })`
- `op_fleet::network::record_beacon(ctx, &Beacon { anchor_id, herd_id, lte_rsrp_dbm, at })`
- `op_fleet::firmware::offer_for(ctx, collar_id)` → the signed manifest, digest and `offer_id`
  for the firmware command, if this collar is ready now (gates read in the reserving write)
- `op_fleet::firmware::still_good(ctx, collar_id, offer_id)` → call right before sending; `false`
  means don't send
- `op_fleet::firmware::record_result(ctx, collar_id, version, &FwResult::{Applied, Confirmed,
  RolledBack, Failed { code }})`
- `op_fleet::firmware::image_bytes(ctx, sha256)` for `<endpoint>/fw/<digest hex>`
- `op_fleet::units::for_collar(ctx, collar_id)` → the unit, with its device key for frames
