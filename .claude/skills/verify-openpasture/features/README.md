# openpasture verification map

The maintained source for proving openpasture's user-facing behaviour on a local stack. Read this, then use the matching feature file as the recipe.

| Feature | File | Journey |
| --- | --- | --- |
| Server health | [health.md](health.md) | `drive health` |
| Collars report to the farm | [collar-reports.md](collar-reports.md) | `drive collar-reports` |
| Per-collar report limit | [report-rate-limit.md](report-rate-limit.md) | `drive report-rate-limit` |
| Sign in and see the farm | [app-shell.md](app-shell.md) | `drive app-shell` |

Not mapped yet: drawing paddocks and sending a boundary (`/api/herds/{id}/boundary`), moves and strip schedules, alerts and the farm webhook, reports, invites and sign-in links, the desktop app (Tauri, macOS only).

## Baseline preconditions

- A stack from `bin/verify launch` on `127.0.0.1:17990`, made from an empty data dir: "Simulated farm", paddocks "Sim 1" and "Sim 2", one simulated herd of 6, and 6 simulated collars reporting.
- `bin/verify doctor` exits 0.
- Never drive an instance this run didn't launch. The garage stack on 7878 and 7991 is the human's.

## Driving conventions

- The app API uses the app token from `stack.env` (`Authorization: Bearer <token>`), as the UI does after the Token prompt.
- The collar endpoints (`/collar/v1/report`, `/boundary`, `/ack`) use a collar's own key from `<run>/farm/collar-sim.json`, as firmware does.
- The browser is reached by aria-labels and visible text, not CSS paths.
- A journey that changes state (rate limits, new rows) runs against a fresh launch when its result depends on a clean start.

## Proof and skip reporting

- Every request and its status and body is in `<run>/evidence/<journey>.jsonl`. Screenshots are `.png` next to it.
- A missing precondition (no Chromium, no build) is a failure with the reason, never a skip that reads as a pass.
