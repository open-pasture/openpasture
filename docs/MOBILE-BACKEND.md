# Mobile backend (staging)

Base URL: `https://op-server-mobile-staging.up.railway.app`

This is the real openpasture server. Except for collar sensor readings explicitly marked simulated, mobile clients must not substitute seeded/demo data.

## Authentication

Sign in with Google through Clerk issuer `https://moved-sparrow-494.clerk.accounts.dev`, then exchange the short-lived Clerk session JWT:

```http
POST /api/auth/clerk
Content-Type: application/json

{"token":"<Clerk session JWT>","name":"Cody","email":"…","label":"Android"}
```

The response is `{"token":"opu_…","user":User,"farm_ready":boolean}`. Store the `opu_` token securely and send `Authorization: Bearer <token>` on every `/api` request. A known Clerk `sub` maps back to the same person. The first Clerk user on an empty server becomes owner; subsequent new users default to manager.

For coordinator-only end-to-end tests, `POST /api/auth/bootstrap` accepts `{"secret":"<OP_BOOTSTRAP_SECRET>","name":"Mobile test","label":"Android test"}` and returns the same shape. Never put this secret in the app or logs.

## Phone onboarding

Use these calls in order (all with the person bearer token):

1. `POST /api/farm` — `{"name":"My Farm","center":[-96.58,39.19]}`.
2. `POST /api/features` — farm boundary: `{"kind":"farm_boundary","name":"Farm boundary","geometry":{"type":"Polygon","coordinates":[[[lon,lat],…,[lon,lat]]]}}`.
3. `POST /api/paddocks` — `{"name":"North","geometry":<GeoJSON Polygon>}`; repeat for each paddock.
4. `POST /api/herds` — `{"name":"Cows","species":"cattle","count":20,"paddock_id":"pad_…"}`.
5. `POST /api/animals` — `{"tag":"101","herd_id":"herd_…"}`.
6. Scan an OC2 card, then `POST /api/fleet/claim` — `{"card":"OC2:…","herd_id":"herd_…","animal_id":"ani_…","name":"101"}`. A typed code may use `{"claim_code":"XXXX-XXXX-XXXX-XXXX",…}`.
7. The hosted simulator sends the first report through `/collar/v1/report`; read it from `GET /api/positions?herd_id=…` and the live socket.

## Mobile feature endpoints

- Initial state: `GET /api/state`; profile: `GET /api/me`.
- Farms and map: `GET|PATCH /api/farm`, `GET|POST /api/paddocks`, `GET|PATCH|DELETE /api/paddocks/:id`, `GET|POST /api/features`, `PATCH|DELETE /api/features/:id`.
- Herds/animals/collars: `GET|POST /api/herds`, `PATCH /api/herds/:id`, `GET|POST /api/animals`, `PATCH /api/animals/:id`, `GET /api/collars`, `GET /api/positions`, `GET /api/tracks`.
- Fence now: `POST /api/herds/:id/boundary` with `{"geometry":<Polygon>}`; status at `GET /api/herds/:id/boundary`.
- Rotation/strip schedules: `POST /api/strips/preview`, `POST /api/schedules`, then `GET /api/schedules`; actions are `/api/schedules/:id/{pause,resume,hold,skip,move-now,time,end}`.
- Decisions: `POST /api/herds/:id/decide`, list with `GET /api/decisions?herd_id=…`; answer `POST /api/decisions/:id/respond` using `{"action":"approve"}`, `{"action":"reject","note":"…"}`, or `{"action":"modify","geometry":<Polygon>,"note":"…"}`.
- Pasture: `GET /api/signals`, `GET /api/analytics/pasture`, `GET /api/land/:paddock_id`, `GET /api/paddocks/:id/heights`.
- Alerts: `GET /api/alerts`, `GET /api/alerts/:id`, `POST /api/alerts/:id/ack`, `POST /api/alerts/:id/resolve`.
- Live feed: WebSocket `wss://op-server-mobile-staging.up.railway.app/api/live?token=<URL-encoded opu token>`. Re-fetch state after reconnect; event payloads are invalidation/progress hints, not the sole record.

Full request/response schemas are in [API.md](API.md) and fleet claim details in [api/fleet.md](api/fleet.md).

## Demo factory collars

These are stable demo-factory credentials. `OP_SIM=1` provisions them as unclaimed inventory on first boot. Claiming one starts realistic drifting reports through the real authenticated `/collar/v1/report` ingest path. The drift follows the herd's applied boundary (then its paddock, then farm center), with simulated battery and health data; low-level cue events are capped at three per 24 hours. The reported `sim` capability labels provenance. Device seeds and claimed v1 bearer keys persist in `/data/mobile-sim-device-keys.json` (0600) across deploys.

| Model | Serial | Claim code | OC2 card |
|---|---|---|---|
| Rook | OPR-ROOK-0001 | `R00K-1001-GRAZ-E001` | `OC2:OPR-ROOK-0001:34ToCB-ZedBSG5VdZ5JwZ23rxMyHEVnk0KizH5Xcn8E:R00K-1001-GRAZ-E001` |
| Rook | OPR-ROOK-0002 | `R00K-1002-GRAZ-E002` | `OC2:OPR-ROOK-0002:L7DE04GA7jM8_f_qG5n27DhJkQq0kOTdiUJKgyfuL9M:R00K-1002-GRAZ-E002` |
| Rook | OPR-ROOK-0003 | `R00K-1003-GRAZ-E003` | `OC2:OPR-ROOK-0003:bf0wjmS2rTpQfxR5O_kcNN9jOs89FQgo8oJzJXIwesw:R00K-1003-GRAZ-E003` |
| Rook | OPR-ROOK-0004 | `R00K-1004-GRAZ-E004` | `OC2:OPR-ROOK-0004:sv2Xj9R9FF4B-anR9ztKkDrmFUIxfMGKBzK233hta1s:R00K-1004-GRAZ-E004` |
| Rook | OPR-ROOK-0005 | `R00K-1005-GRAZ-E005` | `OC2:OPR-ROOK-0005:qlGoQnzBlPb1F49Kwklu1XiHQwt0E9faZbPBJbeXGCI:R00K-1005-GRAZ-E005` |
| Rook | OPR-ROOK-0006 | `R00K-1006-GRAZ-E006` | `OC2:OPR-ROOK-0006:kq7So1kBpM08J--uUsP94qodyPtJBJw9UOIVASeLNBs:R00K-1006-GRAZ-E006` |
| Rook | OPR-ROOK-0007 | `R00K-1007-GRAZ-E007` | `OC2:OPR-ROOK-0007:gFYvvfck4-onrkjy9SKn5EpQVZqudxN7IjPJ7lOelLw:R00K-1007-GRAZ-E007` |
| Rook | OPR-ROOK-0008 | `R00K-1008-GRAZ-E008` | `OC2:OPR-ROOK-0008:ehAKqwbK-QAwRicgH7mCulnx9Dq0NXPjVPNhoMqi0u0:R00K-1008-GRAZ-E008` |
| Starling | OPS-STARLING-0001 | `STAR-2001-GRAZ-E001` | `OC2:OPS-STARLING-0001:LzUzvySqKE48hCU8VzX8BfckDgRVQV3qZ8kN3bWia_k:STAR-2001-GRAZ-E001` |
| Starling | OPS-STARLING-0002 | `STAR-2002-GRAZ-E002` | `OC2:OPS-STARLING-0002:eInT09ps9uueuZtka7eNMt1icN46zc203ADQyVFqV6E:STAR-2002-GRAZ-E002` |
| Starling | OPS-STARLING-0003 | `STAR-2003-GRAZ-E003` | `OC2:OPS-STARLING-0003:qNJl6HkFPaqC0gdc8GB6quKDOs80nkkubDBpn0LTSTg:STAR-2003-GRAZ-E003` |
| Starling | OPS-STARLING-0004 | `STAR-2004-GRAZ-E004` | `OC2:OPS-STARLING-0004:SPbDXUx4UzJAjVK_RdZc5QRJQMpxoJImCmrXbWOoPsE:STAR-2004-GRAZ-E004` |
| Starling | OPS-STARLING-0005 | `STAR-2005-GRAZ-E005` | `OC2:OPS-STARLING-0005:EyDzbUGWikkIA_E_7qPoJ1kvB5ot00C7Wt-UyvoSo80:STAR-2005-GRAZ-E005` |
| Starling | OPS-STARLING-0006 | `STAR-2006-GRAZ-E006` | `OC2:OPS-STARLING-0006:n8WciD0X2L27Wlw5FK63aeMMPzvfR2bSdDpDSEWGatc:STAR-2006-GRAZ-E006` |
| Starling | OPS-STARLING-0007 | `STAR-2007-GRAZ-E007` | `OC2:OPS-STARLING-0007:CqUhFrWLcQ5YzLfiHVejwxnII1Qf93UdWVNbHRsl6uY:STAR-2007-GRAZ-E007` |
| Starling | OPS-STARLING-0008 | `STAR-2008-GRAZ-E008` | `OC2:OPS-STARLING-0008:0eWsLETNJ7Ew7-qpOVTdoF8oQnepI2dLZZ89n-0FqCA:STAR-2008-GRAZ-E008` |

The same cards are scan-ready PNGs under `docs/mobile-cards/`.
