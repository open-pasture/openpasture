# Server health

`/healthz` is what a farmer's monitor, Docker and the desktop app poll. It says whether the server serves, its version, and each background job, with the critical ones marked.

## Sub-features

- `health-ok`: 200 with `status: ok` once the database is open and the jobs run.
- `health-jobs`: `jobs[]` lists each background job with `critical`. `ingest.moves` is critical.
- `health-down`: 503 `down` when a critical job has failed for 5 minutes. This is not driven here, because forcing a job to fail needs a fault the stack doesn't offer. `crates/op-server/tests/ops.rs` covers it.

## How to get to it (user POV)

- `curl http://127.0.0.1:17990/healthz`, or the self-hosting guide's monitor (`docs/SELF-HOSTING.md`).

## Driving it with bin/verify

- **Run.** `bin/verify drive health`.
- **Proof.** `PASS /healthz answers 200 ok`, `PASS the critical ingest.moves job is listed and critical`, and the version. The body is in `evidence/health.jsonl`.

## Gotchas

- While migrations run, `/healthz` answers 503 `migrating` and every other route waits. `launch` returns only after the farm was made, so that's over by then.
