# OpenCollar protocol test vectors

The published copy is `opencollar/protocol/vectors/` (Apache-2.0), and it is the source of truth. openpasture's `crates/op-protocol/vectors/` is a byte-identical copy; its tests fail when the two differ. The firmware copies the files it runs into `opencollar/firmware/tests/host/vectors/`, and opencollar's CI fails when those differ.

The files are written by the Rust reference in `crates/op-protocol` (Apache-2.0): `tests/vectors.rs` for the top-level files, `tests/vectors_v2.rs` for `v2/`. To change them:

```sh
OP_WRITE_VECTORS=1 cargo test -p op-protocol --test vectors --test vectors_v2   # in openpasture
cp -R crates/op-protocol/vectors/. ../opencollar/protocol/vectors/
cp ../opencollar/protocol/vectors/{commands,config,shapes,geofence,slots,cue}.json ../opencollar/firmware/tests/host/vectors/
```

Without `OP_WRITE_VECTORS` the same tests fail when a file is out of date, and check op-protocol against every case.

Each file is one JSON object; cases are one per line. Times in v1 files are RFC 3339 UTC; in `v2/` they are Unix seconds. Coordinates are `[lon, lat]`: degrees with at most 7 decimals in v1 files, integers in units of 10⁻⁷ degrees in `v2/`. Byte strings in `v2/` are lowercase hex.

## Test keys

Ed25519 seeds of 32 consecutive bytes. v1 files use only the server key, seed `00 01 … 1f` (`seed_hex`, `public_key` in base64). `v2/` files list every key under `keys` (seed, public key and kid in hex):

| Name | Seed starts at | Used for |
|---|---|---|
| `server` | `00` | The current server key |
| `next_server` | `20` | A handover's new key; the key that binds an unclaimed collar |
| `owner` | `40` | The owner key |
| `device` | `60` | The collar's device key (its kid is the collar's device id) |
| `anchor` | `80` | An anchor's device key (signs batches) |
| `maker` | `a0` | The maker key (factory records) |
| `release` | `c0` | The firmware release key |
| `new_owner` | `e0` | An owner key replacing `owner` |

These keys are public. Never use them for anything but tests.

## The fence: every protocol version

### shapes.json

`{ slack_m, cases: [{ name, boundary, holes, warn_m, hysteresis_m, limits, ok, code? }] }`

`op_geo::shape::check_rings(boundary, holes, limits, warn_m, hysteresis_m, slack_m)`; the first failing rule is the code. Order and exact math are in `opencollar/protocol/README.md` (Validation): margins, hole count, range, few vertices, many vertices, self-intersection, rings crossing, hole outside, holes overlapping (all exact on e7 integers), outer area < 1 m², hole area < 100 m², gap < 2·warn_m + 2 m (single-precision metres about the outer ring's first vertex). `limits` is `{ outer, holes, hole_vertices, total, slots, slot_bytes }`.

### geofence.json

`{ tolerance_m, cases: [{ name, boundary, holes, point, margin_m, inside, nearest_ring }] }`

The fence built from the rings (double precision, projected about the outer ring's first vertex): `margin_m` is the signed distance to the nearest edge of any ring (+ inside), within `tolerance_m`; `inside` means inside the outer ring and outside every hole; `nearest_ring` is 0 for the outer ring, 1.. for holes. No case sits on a tie.

### slots.json

`{ start, cases: [{ name, limits, herd_id, collar_id, steps, expect }] }`, `expect[i]` being the state after `steps[i]`.

Steps:
- `{ op: "insert", version, effective_at?, now?, collar_id?, herd_id?, vertices? }` offers a verified command (id `bnd_v<version>`, herd `herd_id` or the case's, `collar_id` only when given, a valid shape of `vertices` total vertices, default 4; slot bytes are `192 + 8 · vertices`). `now` is the collar's GNSS time at the insert; without it the collar uses its last fix since boot, if any.
- `{ op: "tick", now }` is a fix at that GNSS time.
- `{ op: "boot" }` reboots: slots stay, the clock is gone until the next fix, and staged slots of another herd are dropped.
- `{ op: "set_herd", herd_id }` is a signed config (v1) or membership (v2) naming that herd taking effect: staged slots of any other herd are dropped with no ack; the active boundary stays.

Expect: `{ acks: [{ version, status, code?, at? }], active, have, free, free_bytes }`. `at` is absent when the collar had no GNSS time (it would use modem network time). `have` is the highest version held (0 with none), `free` the slots left, `free_bytes` the slot bytes left (`null` when `limits.slot_bytes` is 0).

### cue.json

`{ config, cases: [{ name, warn_m, steps, expect }] }`: the cue policy ("only cue a crossing", warning levels, the 20 s cap and 30 s rest, the 10 s outside tone, episodes, track mode). `config` is the cue configuration (`warn_freq_hz`, `outside_freq_hz`, `beep_ms`, `max_active_ms`, `rest_ms`, `outside_max_ms`). Each case starts from boot: unarmed, audio mode.

Steps:
- `{ op: "fix", t_ms, state, margin_m, ring }`: a geofence result (`inside`, `warning`, `outside` or `unknown`, the signed margin, the nearest ring) at `t_ms` on the collar's clock.
- `{ op: "rearm", t_ms }`: a new boundary applied at `t_ms`.
- `{ op: "mode", mode }`: `audio` or `track`.

Expect after each step: `{ cue, armed, episodes }`. `cue` is `null` (silent) or `{ kind, level }` (`warn` 1–4, `outside` 4); `armed` whether a crossing would be cued; `episodes` the episodes that ended at this step, each `{ start_ms, end_ms, ring, cues, max_level, min_margin_m, outcome }`.

## v1 commands (HTTP + JSON)

### commands.json

`{ seed_hex, public_key, max_bytes, max_keys, cases: [{ name, wire, valid, code?, canonical? }] }`

`wire` is the exact text a collar receives. Check it as the collar does (`op_protocol::verify_wire`): size (`too_large` above `max_bytes`), the top-level span scan (`bad_json`: not one object, nested object, arrays deeper than 4, a key with a backslash, a duplicate key, more than `max_keys` keys, trailing text, raw control characters), then the Ed25519 signature over the canonical bytes (`bad_sig`: missing, not a plain base64 string of 64 bytes, or not verifying), then the fields (`bad_json`: wrong types, missing required fields, an id empty or over 64 bytes). `canonical` (valid cases) is what the signature covers: keys sorted bytewise, `sig` removed, value tokens copied verbatim with whitespace outside strings removed.

### config.json

Same shape, plus `collar_id` and `held_version`: the collar's own id and the config version it already holds. Check with `verify_config_wire`, then `ConfigCommand::check(collar_id, held_version)`: ids (`bad_json`), `wrong_collar`, `stale` (version ≤ held), `bad_config` (an interval outside 10-3600 s, `fast_until` without both fast intervals, an endpoint not `https://…` or over 256 bytes).

## v2

The spec is `opencollar/protocol/v2/`; the schema is `v2/opencollar.cddl`.

### v2/messages.json

Header: `keys`, `device_id` (the collar under test, the `device` key's kid), `herd`, `now` (its GNSS time), `nonce` and `claim_code` (its claim state), `limits` (its boundary limits), `claimed_at` (its claim time), `pending_handover` (the digest of the handover it holds pending), `zones` and `lease` (the zones and lease it holds, as signed commands), `image_keys` (the firmware keys its bootloader trusts).

`commands: [{ name, state, clock, hex, json, expect, schema }]`: `hex` is a COSE_Sign1 exactly as received; `json` the payload's JSON view (`null` when it doesn't decode); `expect` is `ok` or the reject code; `clock` false means the collar has no GNSS time yet; `schema` is what the CDDL makes of it (`accept`, `reject`, `skip`). `state` is the collar's: `claimed` (current server key `server`, pending handover key `next_server` staged by the header's handover, owner key `owner`, herd `herd`, claimed at `claimed_at`, holding the header's zones and lease) or `unclaimed` (no keys, no herd). Check in this order, the first failure being the code:

1. Size: over 12 288 bytes is `too_large`.
2. The envelope: tag 18, four elements, protected header `{1: -8, 4: kid}`, an empty unprotected map, a 64-byte signature (`bad_cbor`; another algorithm or a `crit` header is `bad_sig`).
3. The key: the trusted key with that kid (a bind is checked against the `server_key` it carries); none is `unknown_kid`. The signature over `["Signature1", protected, "opencollar/2", payload]` (`bad_sig`).
4. The payload: deterministic CBOR with unsigned integer keys and no tags, a command type, `herd`, `version`, `issued_at`, `device` for config, membership, bind, resend and release, and every known field with its type and range (`bad_cbor`).
5. Who may sign it: a bind only while unclaimed (`claimed`); a confirm only by the pending key naming the current one and the pending handover's digest (`bad_sig`); a rotate handover by the server or owner key; a release, a transfer or an owner command by the owner key (the server key only while there is none); anything else by the server key alone (`unknown_kid`).
6. A bind's claim: the collar's nonce and the proof for its claim code (`bad_claim`).
7. Scope: `device` another collar's (`wrong_collar`); `herd` another herd's, except a membership or bind (`wrong_herd`); `expires_at` at or before `now` (`expired`), or present with no clock (`not_now`).
8. Versions: issued before `claimed_at` (`stale`). No versions are held otherwise.
9. The type's own rules: a boundary's shape against `limits` (v1's codes), a config's ranges, a membership's (`unsupported` for an unknown plan, else `bad_config`), a handover's deadline (`bad_config`), zones' shapes; a trust update's signature by an image key; a type the collar doesn't implement is `unsupported`.
10. A boundary against the held zones and lease (`in_exclusion`; waiting is `ok`).

The slot store comes after, as in `slots.json`.

`documents: [{ name, signer, hex, json }]`: factory records, a firmware manifest, a hello and a trust update, each a COSE_Sign1 by the named key, with the JSON view of its payload.

### v2/records.json

`records: [{ name, hex, json, expect, same_bytes, schema }]`: one uplink record each. Decode the CBOR (`bad_cbor` when it isn't deterministic, has a tag or a map key that isn't an unsigned integer), then the record (`bad_cbor` for a known field of the wrong type or out of range); `expect` is `ok` or the code. `json` is its JSON view. A record type, cue kind or outcome the reader doesn't know is kept, shown as its number; elements past the known ones are ignored, so re-encoding gives back the same bytes only when `same_bytes`. Every record a sender makes is at most `max_record` (109) bytes; a device snapshot comes in parts (`device_part_i_of_n`) that merge back into one.

### v2/link.json

- `frame_key`: the X25519 shared secret between the `device` and `server` keys and the AES-128 frame key derived from it.
- `beacon_key`: the test herd's beacon key.
- `uplinks: [{ name, net, src, fcnt, link, records, hex }]`: sealed uplink frames; open with the frame key and the device id. `records` has `null` for a record that is refused (`one_malformed_record_among_valid`: its neighbours are read).
- `beacons: [{ name, json, hex, expect }]`: check the 8-byte tag with the beacon key (`bad_sig` when it doesn't verify), then read the map between the 9-byte header and the tag.
- `fragments: [{ name, message, max_data, msg, frames, received, need, expect }]`: a signed boundary split into fragment frames; after receiving the fragments listed in `received`, the collar's `need` is `{ from, missing }`. `expect: refused`: a count no command can have; nothing is allocated for it.
- `slots: [{ short, cycle_min, groups, fast, k, g, start_ms, minutes, sends }]`: plan 1 airtime slots: the slot index and group of a short address, when its slot starts, and whether it sends in each GNSS minute (Unix time / 60) with that schedule, fast or not.
- `airtime: [{ sf, bw_khz, bytes, us }]`: time on air of a LoRa packet.
- `hellos: [{ name, hex, signed }]`: a hello frame and the signed hello in it.
- `batches: [{ name, signer, hex, frames, reply }]`: an anchor's signed batch (the frames it carries), and a reply.
- `joins: [{ name, batch, reply, air, collars }]`: two unclaimed collars join at once: their net-0 hellos in the anchor's batch, the server's reply with each one's bind and membership, the fragments the anchor sends to dst 0 (`air`), and for each collar what it keeps (`keeps`: its own bind and membership), the beacon key its membership wraps for it, and its first sealed status (`seq` 0, `fcnt` 0).
- `replay: [{ name, fcnt, accepted }]`: a server's replay window over a sequence of frame counters.

### v2/identity.json

- `claim_codes: [{ input, normalized }]`: claim code normalization (`null` when invalid).
- `claim`: K_claim, `claim_check` and a bind proof for the test collar.
- `card`: the card text and what it holds.
- `manifest`: a signed release manifest, and `install: [{ name, state, window, expect }]`: whether a collar in that state (its class, board, counter, battery, GNSS time, last fix, fence state, episode, pen, fast window, staged boundaries) and window (`effective_at`, its own offset) installs it now (`ok`, `incompatible`, `downgrade` or `not_now`).
- `test_boot: [{ test, no_fix_before, then }]`: what follows a test boot: confirm, or roll back and whether to refuse the digest.
- `trust_store` and `trust_updates: [{ name, signer, hex, before, expect, after }]`: trust updates applied in turn to a bootloader trusting both release keys.
- `counters: [{ name, copies, durable, resume_at }]`: a counter's two-copy reservation record in flash, what it reads as, and where the counter resumes; one write torn by a power cut.
- `zones`: the zones the claimed collar holds and their digest. `overlaps: [{ name, outer, holes, zone, overlaps }]` and `activation: [{ name, boundary, held_epoch, lease, now, then }]` (`activate`, `wait_zones`, `wait_lease`, `wait_clock`, `in_exclusion`).
- `v1_herd`: a v1 herd id and the v2 herd id derived from it.
