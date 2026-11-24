---
name: verify-openpasture
description: >-
  Launch and drive a real local openpasture stack (op-server, collar-sim's
  simulated farm, the UI the server serves) the way a farmer and the collars do,
  and capture evidence. Use to prove a server, collar-protocol or UI change
  works, to reproduce a bug end to end, or when asked to verify openpasture.
  Local only: it refuses any staging or production URL before a connection.
---

# Verify openpasture

Everything here drives **this machine only**. `bin/local_only.py` is the one URL check, and every entry point runs it first: `bin/verify`, `bin/drive.py` and `bin/ui.ts`. A base URL passes only if it is `http://127.0.0.1|localhost|[::1]:<port>` and the port is not 7878, where a real farm runs. Anything else exits 3 before any DNS lookup or connection, staging (`op-server-staging*.up.railway.app`) and `openpasture.dev` included. Never work around it. If something must be checked on staging, ask the human.

Paths below are relative to the repo root, with `V=.claude/skills/verify-openpasture/bin/verify`.

## Launch

```sh
$V build     # ui/dist (the server embeds it), then cargo build -p op-cli -p collar-sim
$V launch    # a new stack on 127.0.0.1:17990 (OP_VERIFY_PORT to change)
```

`launch` runs `collar-sim farm --port 17990 --data-dir <run>/farm --count 6`. That starts the real `openpasture serve` on an empty data dir, migrations included. Through the app API it makes "Simulated farm" with two paddocks and a simulated herd, then links 6 collars that report over the real collar endpoints. Ready means `collar-sim.log` printed `simulated farm at …` and the collars' keys are in `<run>/farm/collar-sim.json`. `launch` waits for both (up to 120 s), mints the app token with `openpasture token`, and writes `<run>/stack.env`.

- Binaries come from `$OP_BIN`, else `$CARGO_TARGET_DIR/debug`, else `target/debug`. To verify another commit, build it in its own worktree and point `OP_BIN` at that build.
- Runs go to `target/verify-openpasture/<UTC stamp>-<port>/`, with `target/verify-openpasture/current` pointing at the live one. Two stacks can run side by side with different `OP_VERIFY_PORT` and `OP_VERIFY_DIR`. `launch` refuses a port it didn't take, and refuses a second launch over a running `current`.
- The garage stack (`~/bin/opdev`: 7878 dev, 7991 sim farm, 5173 UI) belongs to the human. Don't launch on its ports and don't drive it unless asked. Driving 7991 also needs its data dir for collar keys.

## Doctor

```sh
$V doctor
```

Read-only. It checks that `/healthz` is 200 `ok`, the app token is accepted, the farm is the "Simulated farm" launch made, and the data dir carries `SIMULATED-FARM`. Exit 2 means don't drive this instance: relaunch. Run it first whenever anything looks off.

## Drive

```sh
$V drive health              # /healthz: ok, version, the critical ingest.moves job
$V drive collar-reports      # the herd's collars link, report fixes, show as simulated
$V drive report-rate-limit   # one collar key: 30 reports a minute, however its header is spelled
$V drive app-shell           # Chromium: Token prompt → sign in → herd, paddock, "Simulated collars"
$V all                       # doctor, then all four
```

`bin/drive.py` speaks HTTP with the app token (the browser's path) and with collar keys (the collars' path), never through test-only routes. `bin/ui.ts` runs headless Chromium (`/usr/bin/chromium`, or `$CHROME`) with `playwright-core` from `bin/package.json`. `drive app-shell` runs `bun install` there the first time. It drives by stable handles: the `Token` input's aria-label and visible text. Each journey prints `PASS`/`FAIL` lines. Exit 0 means proved, 1 means the behaviour is wrong, 2 means the stack isn't fit, and 3 means the URL was refused. `features/` maps each journey: what a user does, how to reach it, and what end state proves it.

New journey: add a function to `bin/drive.py` (or a `bin/ui.ts` step) and a `features/<name>.md`, then run it once against a fresh launch before you rely on it.

## Evidence

Each journey writes `<run>/evidence/<journey>.jsonl`: every request (method, path, collar key masked) with its status and body. `app-shell` adds screenshots (`app-shell-1-token.png`, `app-shell-2-farm.png`) and `app-shell.json`. `collar-sim.log` holds the farm and server start, and `build` names the binaries under test and the skill tree's commit. Proof standards:

- Drive the real user path: the app API with the app token, the collar endpoints with a collar's key, the UI in a browser. No internal setters, no test routes.
- Capture the action and the resulting state, such as the request that should be refused and the status it got, not only a final screen.
- A green `cargo test` is not this proof, and this is not a substitute for `cargo test`.
- A bug reproduction shows the same journey failing on the commit before the fix and passing after, each on a fresh launch.

## Cleanup

```sh
$V stop
```

It stops only the process group `launch` started (collar-sim and the server it spawned) with SIGINT, then SIGKILL after 10 s, and deletes the run's `farm/` data dir and `stack.env`. `<run>/evidence/` stays. Never kill by process name: the garage stack runs the same binaries. After a failed launch, run `stop` too.
