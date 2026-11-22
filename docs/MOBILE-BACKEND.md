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

The stable Rook (LTE-M/GPS) and Starling (LoRa mesh) card table and scan-ready PNGs will live here and in `docs/mobile-cards/`. Initial deploy work is in progress; do not invent codes or use seeded app fallbacks meanwhile.
