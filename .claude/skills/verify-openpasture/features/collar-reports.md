# Collars report to the farm

A collar linked to a herd posts its fixes to `/collar/v1/report` with its own key. The farm's state shows each collar with a recent fix. Simulated collars are marked simulated everywhere.

## Sub-features

- `link`: `POST /api/collars` with a herd gives a collar its key. collar-sim does this during launch.
- `report`: fixes arrive and the collar shows a last fix.
- `simulated-marking`: a simulated herd's collars carry `simulated: true`.

## How to get to it (user POV)

- A farmer links collars in the app (Herd → collars). Each collar then reports by itself.
- In this stack, `collar-sim farm` links 6 and runs them.

## Driving it with bin/verify

- **Run.** `bin/verify drive collar-reports`.
- **What it does.** Reads `/api/herds` with the app token, then polls `/api/collars` for up to 90 s until every head has a linked collar with a `last_fix`.
- **Proof.** `PASS every collar has reported a fix (6/6)` and `PASS every collar is marked simulated`. The polls are in `evidence/collar-reports.jsonl`.

## Gotchas

- collar-sim ramps its collars in, so the first poll may show fewer than 6 with fixes. That's why the journey waits.
- `/api/state` carries no collars, and the herd JSON has no `simulated` field. Collars come from `/api/collars` (`last_fix.point`, `simulated`, `herd_id`).
