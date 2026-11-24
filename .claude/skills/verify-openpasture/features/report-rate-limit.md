# Per-collar report limit

One collar key may post 30 reports a minute (`crates/op-server/src/limits.rs`, `REPORTS_PER_MINUTE`). The 31st gets 429 with `Retry-After`. The limit belongs to the collar, so another spelling of the same `Authorization` header (`bearer <key>`, `Bearer  <key>`, `BEARER <key>`) counts against the same collar. Before 136098e (Astra 02 F16) each spelling was a new collar, so a key could post without limit.

## Sub-features

- `limit`: the 31st report in a minute from one key is 429.
- `same-collar`: header spellings the collar endpoints accept share that count.

## How to get to it (user POV)

- Collar firmware posts reports. A faulty or abused collar posts too often, and the server must hold it to the limit.

## Driving it with bin/verify

- **Run.** `bin/verify drive report-rate-limit`.
- **What it does.** Takes the first collar's key from `<run>/farm/collar-sim.json`. It posts `{"fixes": []}` reports with `Bearer <key>` until one is 429 (at most 45). Then it posts once with each other spelling.
- **Proof.** `PASS the canonical header reaches the limit` and three `PASS '<spelling>' is the same collar and still limited (got 429)`. Statuses are in `evidence/report-rate-limit.jsonl`.
- **Reproduction.** On a build of `136098e^`, the spellings get 200 and the journey fails. On origin/main it passes.

## Gotchas

- collar-sim's own reports with that key count toward the 30, so the limit can arrive before 30 journey reports. That's still the limit.
- The key stays limited for a minute afterwards, so its simulated collar gets 429s in `collar-sim.log`. Run this journey last, or relaunch before the others.
