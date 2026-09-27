# Protocol v1 test vectors

Written by `crates/op-protocol/tests/vectors.rs` (`OP_WRITE_VECTORS=1 cargo test -p op-protocol --test vectors`); the same test fails when these files and the generator disagree, and checks op-protocol against every case. The firmware copies the `.json` files byte-identical into `opencollar/firmware/tests/host/vectors/`.

Times are RFC 3339 UTC. Coordinates are `[lon, lat]`, at most 7 decimals. Signing key: Ed25519 seed bytes `00 01 02 … 1f` (`seed_hex`), public key `public_key` (base64). Each file is one JSON object; `cases` holds one case per line.

## commands.json

`{ seed_hex, public_key, max_bytes, max_keys, cases: [{ name, wire, valid, code?, canonical? }] }`

`wire` is the exact text a collar receives. Check it as the collar does (`op_protocol::verify_wire`): size (`too_large` above `max_bytes`), the top-level span scan (`bad_json`: not one object, nested object, arrays deeper than 4, a key with a backslash, a duplicate key, more than `max_keys` keys, trailing text, raw control characters), then the Ed25519 signature over the canonical bytes (`bad_sig`: missing, not a plain base64 string of 64 bytes, or not verifying), then the fields (`bad_json`: wrong types, missing required fields, an id empty or over 64 bytes). `canonical` (valid cases) is what the signature covers: keys sorted bytewise, `sig` removed, value tokens copied verbatim with whitespace outside strings removed.

## config.json

Same shape, plus `collar_id` and `held_version`: the collar's own id and the config version it already holds. Check with `verify_config_wire`, then `ConfigCommand::check(collar_id, held_version)`: ids (`bad_json`), `wrong_collar`, `stale` (version ≤ held), `bad_config` (an interval outside 10-3600 s, `fast_until` without both fast intervals, an endpoint not `https://…` or over 256 bytes).

## shapes.json

`{ slack_m, cases: [{ name, boundary, holes, warn_m, hysteresis_m, limits, ok, code? }] }`

`op_geo::shape::check_rings(boundary, holes, limits, warn_m, hysteresis_m, slack_m)`; the first failing rule is the code. Order and exact math are in `crates/op-geo/src/shape.rs`: margins, hole count, range, few vertices, many vertices, self-intersection, rings crossing, hole outside, holes overlapping (all exact on e7 integers), outer area < 1 m², hole area < 100 m², gap < 2·warn_m + 2 m (single-precision metres about the outer ring's first vertex). `limits` is `{ outer, holes, hole_vertices, total, slots, slot_bytes }`.

## geofence.json

`{ tolerance_m, cases: [{ name, boundary, holes, point, margin_m, inside, nearest_ring }] }`

The fence built from the rings (double precision, projected about the outer ring's first vertex): `margin_m` is the signed distance to the nearest edge of any ring (+ inside), within `tolerance_m`; `inside` means inside the outer ring and outside every hole; `nearest_ring` is 0 for the outer ring, 1.. for holes. No case sits on a tie.

## slots.json

`{ start, cases: [{ name, limits, herd_id, collar_id, steps, expect }] }`, `expect[i]` being the state after `steps[i]`.

Steps: `{ op: "insert", version, effective_at?, now?, collar_id?, herd_id?, vertices? }` offers a verified command (id `bnd_v<version>`, herd `herd_id` or the case's, `collar_id` only when given, a valid shape of `vertices` total vertices, default 4; slot bytes are `192 + 8 · vertices`). `now` is the collar's GNSS time at the insert; without it the collar uses its last fix since boot, if any. `{ op: "tick", now }` is a fix at that GNSS time. `{ op: "boot" }` reboots: slots stay, the clock is gone until the next fix.

Expect: `{ acks: [{ version, status, code?, at? }], active, have, free, free_bytes }`. `at` is absent when the collar had no GNSS time (it would use modem network time). `have` is the highest version held (0 with none), `free` the slots left, `free_bytes` the slot bytes left (`null` when `limits.slot_bytes` is 0).
