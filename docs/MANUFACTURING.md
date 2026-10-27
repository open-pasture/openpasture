# Building collars with openpasture

How to run a test station against an openpasture server: for us building the first 1,000
OpenCollar units, and for anyone building their own collars or other equipment on the OpenCollar
protocol. The API is in [`api/fleet.md`](api/fleet.md); the hardware side (fixture, pogo pins,
instruments) is in `opencollar/docs/TEST-FIXTURE.md`.

## The line

Per unit, from `opencollar/docs/LORA-V1-PLAN.md`:

1. **Board on the fixture** (about a minute): flash, rails, sleep current, GNSS, LoRa, IMU,
   buzzers, hall sensor. The station registers the unit and posts the run.
2. Into the shell: board, pack, panel lead, pods. Gasket, lid, screws.
3. Leak test.
4. **Scan, pair and check in**: the station provisions the unit and prints its label.
5. Strap, counterweight, box. The card goes in the box.

Two places touch openpasture: the fixture at step 1 and provisioning at step 4.

## Setting up

1. **A server.** Any openpasture server works: the desktop app for a few units, a headless
   `openpasture serve` on an always-on machine for a production line. Ours is the maker's
   server; a DIY builder's is usually their farm's.
2. **Settings > Advanced > Fleet** turns the Fleet view on (owners and managers).
3. **Fleet > Stations** (owner): add a station per fixture PC. Its token is shown once; put it in
   the station's config. A station token can register units, fetch test plans, post test runs,
   provision and print labels, nothing else. Revoke it when the PC is retired or lost.
4. **Provisioning** (same page): the LoRa region for where the units go (US915 by default), and
   the maker name that goes in factory records (OpenCollar for us; your name for your builds).
   The page shows this server's **maker key**: give it to the farms you ship to, so they see your
   factory records as verified.
5. **Fleet > Test plans**: the server ships plans for `node` and `anchor`. Check the limits
   against your fixture, the RSSI limits above all (they depend on your reference radio and how
   far it is from the board), and save. Each save is a new version; old runs keep theirs. A class
   of your own (`trough`, `gate`…) gets a plan the same way.

## What the station does per unit

All requests carry `Authorization: Bearer ops_…`.

```
POST /api/fleet/units                         register: serial, class, board, firmware, device key, radios
GET  /api/fleet/test-plans/node               the steps and limits to run
POST /api/fleet/units/<serial>/tests          every step's status and value; the server judges the run
POST /api/fleet/units/<serial>/provision      only after a passing run: claim code, card, factory record
GET  /api/fleet/units/<serial>/label.svg      the 50 × 25 mm label for the printer
```

- **Send a `run_key` with every run** (your own id for it, e.g. `station/serial/start-ms`) and
  the unit's `generation` from the registration reply. A station that lost its connection can
  resend the run safely: the same key is the same run. If the unit was sent back or retired
  while it was on the fixture, the server refuses the run and you test it again.
- **What counts is the latest run by the server's order** in the unit's current generation. A
  pass that arrives after a later run of the same unit (a second station, a delayed retry) is
  kept but doesn't count, so an old pass can never overrule a newer failure. A unit back from
  RMA needs a fresh pass: nothing from before counts.

- **The device key** is made on the unit, by its firmware, the first time it boots on the
  fixture; the station reads the public half over the test UART. The private half never leaves
  the unit.
- **Registering is idempotent.** Re-seating a board and registering it again is fine. A
  different key for the same serial is refused: it means a new board or a wiped one, and only the
  owner can accept it (`force`), after which it goes back through the fixture.
- **The server judges the run.** A measured value outside the plan's limits fails even if the
  station said pass, and a required step that is missing or skipped fails the run. The station
  can stop at the first failure and post what it has: the missing steps show as missing.
- **The claim code** is 16 characters the fixture draws and writes to the unit (or the server
  draws at provisioning and the station writes it). The card and label carry it. The server keeps
  it until the unit is in service on a farm, then forgets it.
- **The factory record** is the unit's birth certificate: serial, class, board, device key,
  firmware, maker, claim check, radios, the firmware keys its bootloader trusts, and its test.
  If the fixture signs its own (with a maker key it holds), send it with the registration;
  otherwise this server signs one with its maker key at provisioning.

## The bridge as a station today (firmware 0.2)

`collar-bridge factory --server … --station-token … --port … --class node --hw-rev N1.0 --fw 0.2.0`
runs the plan over the console, one screen per unit ending in one word and a "next:" line, and
appends every run to `factory-results/runs.jsonl`. What it can and can't do now:

| Step | Today |
|---|---|
| `fw_flash` | pass only when the unit runs the `--fw` version; fail on another version or garbage; skip without `--fw` |
| `gnss_fix` | pass only for a finite fix on Earth, not 0,0, accuracy 0–50 m, ≥ 4 satellites when it says |
| `gnss_ttff` | timed on the collar's clock from `GNSS started` to the first fix; fail if the fix is "before" the start (it rebooted); skip if the station didn't see it boot |
| instruments (`rail_*`, `sleep_current`, `charger`, `fuel_gauge`) | skip unless typed in with `--operator` |
| firmware-only steps (`imu`, `buzzer_*`, `lora_*`, `ble`, …) | skip: no console command yet |

Every unit then ends **CAN'T REGISTER**: firmware 0.2 prints no device key, so the server can't
register it, and nothing reaches the server (exit code 3). Once firmware prints its key, a
passing unit ends **PASS, NOT PROVISIONED**: no firmware can store a claim code yet, so the
station doesn't provision it (the server would otherwise say "provisioned" for a unit that
doesn't hold its code). Exit codes: 1 fail or no answer, 2 the server didn't record it, 3 the
firmware can't go further yet; never 0 until a unit can go all the way.

## Labels and cards

The label (from the server, or **Fleet > Units > a unit > Label**) shows the serial, class and
board, the claim code on two lines of two groups, and a QR code with the card text
`OC2:<serial>:<device key>:<claim code>`. Every line fits the label at the longest serial and
code; print and scan one at real size before a batch. Print the same QR on the card in the box. Several
labels at once: `#/print/fleet-labels/<serial>,<serial>,…`.

## Watching the batch

**Fleet > Build** shows, per test plan version and time range (a plan edit is a new version; runs
judged by other versions are counted apart, never mixed in): first-pass and final yield, each step's
pass, fail, skip and missing counts with the spread of measured values, the steps failing most,
and each station's runs, units, failures and median cycle time. A step that starts failing on one
station and not another is usually the fixture, not the boards.

**Fleet > Units** lists every unit by serial with its state and last test, 200 at a time with the
total ("200 of 1,000 units") and **Show more**. A unit's page starts with one line saying what
happens next for its state, then has
every run step by step, its history (who registered, tested, provisioned and claimed it, and
when), its device keys and its factory record. When a collar comes back from a farm, this is where
you look up how it tested the day it was made. **Send back (RMA)** puts it back on the bench;
a fresh passing run and provisioning return it to the same collar. **Retire** is final unless
you send it back (RMA) again.

## Claiming

A farm claims a unit into a herd with the card (scanned) or the claim code (typed), in
**Fleet > Units** or through `POST /api/fleet/claim`. The unit becomes a collar in that herd. A
server that never saw the unit registers it from the card; with the maker's factory record it
also knows the class, board and firmware, and whether the maker is one it trusts.

A card's claim code is checked against the factory record's claim check whenever there is one,
so a typo is refused before any airtime. A card claimed with no record says so: the collar
checks the code when it first reports.

The collar's first report puts the unit in service. Until then its page on the herd panel and
the animal page says it is waiting, and the console shows where it should report.

A collar that was **reset** on the farm (its keys wiped) is claimed again by scanning its card:
the code is checked against the factory record, the same collar keeps its history, herd and
animal, and it gets a new key (the old one stops working).

## Firmware

Updates are automatic for farmers (Settings > Collars > Firmware updates: automatic or paused).
A release is a manifest signed by the OpenCollar release key or by an image key the owner enrolled
under **Fleet > Firmware**; builders who run their own firmware enrol theirs. Releases go out in
stages: one collar of the first herd, the rest of that herd a day after it confirms, every herd
three days in. Nothing installs during a move or an escape or below 40 % battery (checked when
the offer is made and again right before it is sent), a rollback pauses the rollout, a collar
that never answers is offered again with a growing wait and fails after five tries, and collars
that don't take updates over the air say so.

## Waiting on the collar ingest

- Delivering a release to a collar needs the collar ingest to send the protocol's firmware
  command; until a collar reports the `firmware` capability it shows as unable to update over
  the air.
- Getting this server's key onto a v2 collar is the bind command, sent by the collar ingest when
  the collar's hello arrives. Today's firmware (protocol v1) is set up with the `provision` line
  the claim returns, over USB.
