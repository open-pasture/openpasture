# Agents

## Rules and what enforces them

Each rule here is a mistake agents made more than once. A check now fails on it, so this table is a map, not the guard. When an operator corrects a mistake, fix it, then add its rule here with what enforces it. If a rule is here and nothing enforces it, enforce it in the same change.

| Rule | Enforced by | Run locally |
| --- | --- | --- |
| op-protocol's vectors match opencollar's at a named commit. Move them only with `scripts/sync-vectors.sh`, which copies the files and writes `crates/op-protocol/OPENCOLLAR_REF` together. | CI `protocol vectors`: `scripts/compare-vectors.sh` fails on any difference, and on a missing or empty pin (no skip). The daily `vectors drift` workflow compares with opencollar's main. | `scripts/compare-vectors.sh ~/code/opencollar` (add `origin/main` for the drift check) |
| A test never writes out a date on or after today. Derive it from the clock (`saturday_before_next_fall_back` in `crates/op-engine/tests/schedules.rs`). Pure calendar math that never meets the clock carries `fixed-date: <why>` on its line. | CI `repo rules`: `scripts/check-test-dates.py` checks the lines a change adds. | `scripts/check-test-dates.py` |
| Scripts, soak runs and verification drivers never reach staging or production. Every server address goes through `local_only()`/`local_hostport()` (`scripts/local_only.py`) or `localOnly()` (`scripts/local-only.mjs`) before the first request. | CI `repo rules`: `scripts/check-local-only.py`, and the guards' tests (`scripts/test_local_only.py`, `scripts/local-only.test.mjs`). | `scripts/check-local-only.py` |

Exceptions go on the offending line with a reason, an expiry date and the name of the person who approved them.
