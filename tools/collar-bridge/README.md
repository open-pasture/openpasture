# collar-bridge

A real OpenCollar on USB serial, talking to an openpasture server. Three jobs:

- **bridge**: a collar on the bench reports to the server as if it had its own modem. Its console
  log becomes `/collar/v1/report`; boundaries and configs from the server are written back to its
  console; its answers are posted as acks. Firmware 0.2 has no network code, so this is how every
  server feature gets tested on real GNSS and a real buzzer today.
- **capture and replay**: every serial line and HTTP exchange is appended to a capture file;
  `replay` sends a captured session again to a test server.
- **factory**: the test station beside the fixture. One unit after another through the test plan
  for its class, results to the server, provisioning on a pass.

It only ever talks to real hardware. For emulated collars use `simulation/collar-sim`.
macOS and Linux (the port is opened with termios). AGPL-3.0, like the rest of this repo.

```sh
cargo build --release -p collar-bridge
target/release/collar-bridge ports          # serial ports that look like a collar
```

## On the bench

What you need: a collar on firmware with GNSS time on its fix lines (`at=` at the end of each
`fix` line; added after 0.2.0, see below), the server running (the desktop app or
`openpasture serve`), and the collar linked in a herd.

**1. Link the collar.** In the app, or with the API:

```sh
curl -s -X POST http://127.0.0.1:7878/api/collars/bulk \
  -H 'content-type: application/json' \
  -d '{"herd_id":"hrd_…","items":[{"name":"Bench 1"}]}' > link.json
```

The response holds the collar key, shown once. Keep `link.json`: the bridge reads the key and the
provisioning from it. A single link (`POST /api/collars`, `rekey`), a fleet claim response
(`POST /api/fleet/claim`, its `v1` credentials) or a card payload (`{"v":1,…}`) work the same.

**2. Provision and bridge.** Firmware 0.2 stores only an `https://` endpoint, which is where the
collar will report once it has a modem. The bridge itself talks to `--server`, so the bench needs
no https: give the collar its final public URL with `--endpoint`.

```sh
collar-bridge bridge --port /dev/cu.usbmodem1101 --server http://127.0.0.1:7878 \
  --provision link.json --endpoint https://farm.example.com/collar/v1
```

A collar already provisioned as that collar isn't provisioned again (that would wipe its
boundaries); `--force` does it anyway. A collar provisioned some other way needs only its key:

```sh
COLLAR_KEY=… collar-bridge bridge --port /dev/cu.usbmodem1101 --server http://127.0.0.1:7878
```

**3. Watch it work.**

```text
09:14:02 connected to /dev/cu.usbmodem1101
09:14:02 collar col_01… herd hrd_01… firmware 0.2.0: boundary none, holds [], config none
09:14:03 report: 0 fixes, 0 cues, 0 episodes; server's latest none
09:14:03 config v1 taken: report every 60 s, download every 60 s
09:15:03 report: 58 fixes, 0 cues, 0 episodes; server's latest none
09:15:41 boundary v1 (bnd_01…, 1406 bytes) to the collar
09:15:42 boundary v1 applied
09:15:42 ack v1 applied posted
09:16:21 cue warn level 2 at 3.1 m (v1)
```

Draw a boundary in the app: it reaches the collar at the next download (60 s, or the fast
interval the server's config sets during a move), the collar verifies the signature and applies
it, and the app shows it applied. Walk the collar across the line: the cue and the episode show
in the app after the next report. Per collar, the server keeps the last 64 exchanges:
`GET /api/collars/:id/wire` (see `docs/API.md`, "Wire log").

Unplug the collar and the bridge waits for it; fixes it printed before stay queued. If the server
is down, reports are kept (up to 10,000 fixes) and sent when it is back. A report the server
refuses (4xx) is shown with the reason and not resent.

**Bench checks worth running**

- Draw a boundary, watch `applied`, cross it, see the cue in the app.
- Stage a strip (a schedule), stop the bridge, and watch the collar open it on GNSS time by
  itself (its log says `staged boundary vN now in force`). Start the bridge again: the next
  report's slots tell the server.
- Restart the server mid-move: the bridge keeps the fixes and carries on.
- Tamper with nothing: the signature check is the collar's own; a `rejected N bad_sig` would mean
  the collar holds another server's key.

### What the bridge does and doesn't make up

- **Times are the collar's.** Fixes, cues and acks carry the GNSS UTC the collar printed. A fix
  line without a time is dropped, not stamped (the bridge says so once). Between fixes the
  collar's clock is the last fix's time carried forward, as the firmware itself does. An ack the
  collar made before its first fix has no time; that ack, and only that one, is stamped with the
  computer's clock, and the terminal says so.
- **An episode ends at the fix that printed it.** The collar logs an episode after the fix that
  closed it, with its length; the bridge sets `end` to that fix's time and `start` to `end` minus
  the length.
- **Caps and limits.** Firmware 0.2 doesn't print its caps. The bridge reports every 0.2 cap
  (`holes slots collar_id cue_mode episodes config`) and the V0 limits for firmware 0.2 and later,
  nothing for older firmware.
- **Cadence.** The bridge reports and downloads at the intervals of the config the collar took
  through it. After a bridge restart it uses 60 s until the server sends a newer config.
- **Acks for boundaries it didn't deliver.** A staged boundary that comes into force while the
  bridge runs is acked only if this bridge delivered it (it needs the command id). Otherwise the
  next report's `slots` tell the server.
- No battery, cell or C/N0: firmware 0.2 doesn't measure them, so the report leaves them out.

## Capture and replay

`bridge` writes `capture-<date>-<time>.jsonl` in the current directory (`--capture <file>` to
choose, `--no-capture` for none). One JSON object a line, flushed as it happens:

```json
{"kind":"serial","t":"…","dir":"in","line":"[00:01:02.345,678] <inf> opencollar: fix 38.120000,-92.410000 …"}
{"kind":"http","t":"…","method":"POST","path":"/collar/v1/report","body":"{…}","status":200,"reply":"{…}","ms":12}
{"kind":"note","t":"…","text":"port gone: Device not configured"}
```

The collar key is never written: it travels in a header that isn't kept, and the key in a
`provision` line is replaced with `[redacted]`.

```sh
collar-bridge replay capture-20261026-091402.jsonl                        # list what was sent
collar-bridge replay capture-20261026-091402.jsonl \
  --server http://127.0.0.1:7993 --key "$COLLAR_KEY" [--pace] [--out replay.jsonl]
```

Each request is sent again in order and printed with the status it got then and now. Command ids
in boundaries and acks belong to the server they were captured against, so on another server
those differ; use a throwaway server (`openpasture serve --data-dir /tmp/x --port 7993`) with
the collar linked, and its key.

## Factory

```sh
STATION_TOKEN=ops_… collar-bridge factory --server https://ops.example.com \
  --port /dev/ttyACM0 --class node --hw-rev N1.0 --fixture fx-01
```

The station token comes from `POST /api/fleet/stations` (owner); it can only run the station
routes (`docs/api/fleet.md`). For each unit:

1. Scan or type its serial; plug it in.
2. The station reads the boot, asks `status`, and waits for a GNSS fix (`--gnss-timeout`, 120 s).
3. Each plan step gets `pass`, `fail` or `skip` with a reason:
   - `fw_flash`: the firmware answers on the console.
   - `gnss_fix`: a valid fix within the timeout.
   - `gnss_ttff`: GNSS start to first fix on the collar's own clock, when the station saw it boot
     (power it with the station listening); otherwise `skip`.
   - Rails, sleep current, charger, fuel gauge: the fixture's instruments. None are wired to
     this tool, so they are `skip` unless you run with `--operator` and type the meter reading.
   - C/N0, external flash, IMU, Hall, buzzers, LoRa, BLE, LTE, SIM: `skip`, "the collar's firmware
     has no console command for this yet". `--operator` asks for checks (y/n) and readings.

   A step is never sent as a pass it didn't earn. The server judges the run against the plan's
   limits; a required step that was skipped fails the run.
4. The unit is registered, the run posted, and the server's verdict shown. On a pass it is
   provisioned on the server and its claim code and card text are printed. Anything the server
   gives for the console is written to the unit (firmware 0.2 has no command to store a claim
   code, so today nothing is).
5. Unplug it, and the next serial is asked for.

One screen per unit:

```text
────────────────────────────────────────────────────────────────
 OC1-N-000123             FAIL
   pass    fw_flash                   firmware 0.2.0 answers on the console
   skip    rail_3v3                   measured by the fixture's instruments; none are wired …
   pass    gnss_fix                   38.120000,-92.410000 ±3.2 m, 9 sats
   pass    gnss_ttff        31.5 s    GNSS start to first fix, on the collar's clock
 failed: rail_3v3
────────────────────────────────────────────────────────────────
```

Every run is appended to `factory-results/runs.jsonl` whatever the server says, so a dead network
loses nothing.

**Today's firmware can't be registered.** The server registers a unit only with the device key
the unit makes and prints (protocol v2). Firmware 0.2 has none, so the station runs the steps,
shows them as `NOT RECORDED`, and keeps them in `runs.jsonl` only.

## Firmware log lines it reads

From `opencollar/firmware/src/main.c` and `app.c`, Zephyr's prefix
`[hh:mm:ss.mmm,uuu] <inf> opencollar: ` on log lines, none on console replies:

```text
fix 38.120000,-92.410000 acc=3.2m sats=9 v59 state=warning margin=2.5m ring=0 cue=warn vol=3 at=2026-10-26T15:04:05Z
fix 38.120000,-92.410000 acc=3.2m sats=9 (no boundary) at=2026-10-26T15:04:05Z
episode: crossed ring=0 cues=5 max=4 min_margin=-1.2m 15000 ms
ack v59 applied at 2026-10-26T15:04:05Z (1 pending)
status            → collar <id> herd <id> fw 0.2.0 config 3 active 59 slots 59 60
provision {…}     → ok <collar id> | error <code>
boundary {…}      → applied N | received N | rejected N <code> | error <code>
config {…}        → ok N | error <code>
```

`at=` is the fix's GNSS UTC; firmware before it prints none, and the bridge then reports no fixes.
The test fixture `tests/fixtures/bench-session.log` is a transcript in this format, written from
the firmware's format strings. Replace it with a real capture's serial lines after the first bench
session.
