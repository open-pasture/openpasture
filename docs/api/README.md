# openpasture APIs

openpasture is one client of the OpenCollar protocol, and a server other software can drive.
Pick the page for what you're building.

| You're building | Read | Where |
| --- | --- | --- |
| A collar, firmware, or a server that talks to collars | The device protocol: messages, signing, test vectors | [opencollar `protocol/`](https://github.com/open-pasture/opencollar/tree/main/protocol) (v1 today; v2, LoRa with anchor collars, in draft) |
| A collar against an openpasture server | What openpasture does on top of the protocol: collar keys, slot picking, config delivery, acks | [`../API.md` § Device endpoints](../API.md#device-endpoints-collars) and [§ Protocol v1 on the server](../API.md#protocol-v1-on-the-server-op-ingest) |
| An app, dashboard or integration | The HTTP API: farm, paddocks, herds, animals, collars, boundaries, decisions, analytics, reports | [`../API.md`](../API.md) |
| Something that follows the herd live | The `/api/live` WebSocket frames | [`../API.md` § Server and live feed](../API.md#server-and-live-feed-op-server) |
| An AI agent that runs the farm | The MCP server at `/mcp` and its tools | [`../API.md` § MCP tools](../API.md#mcp-tools) |
| A test station, provisioning tool or bridge | Units, factory tests, provisioning, firmware rollout, herd network | [`fleet.md`](fleet.md) |

## Conventions

These hold for every openpasture HTTP API:

- JSON bodies. Geometry is GeoJSON, `[longitude, latitude]`, WGS 84. Times are RFC 3339 UTC.
- IDs are prefixed ULIDs (`farm_…`, `pad_…`, `herd_…`, `col_…`, …).
- Errors are an HTTP status plus `{ "error": "message" }`.
- Auth: `/api/*` and `/mcp` take the app token or a person's token as
  `Authorization: Bearer …`; `/collar/v1/*` takes a collar's own key; `/v1/*` takes hosted
  keys. The full rules, including what counts as a local request, are at the top of
  [`../API.md`](../API.md).

## Stability

The device protocol is versioned on its own (`/collar/v1/…`); a change that breaks a collar
gets a new protocol version. The HTTP API under `/api/*` is used by openpasture's own UI and
can still change between releases; changes are listed in `CHANGELOG.md`. A stable,
versioned subset for integrators is planned.
