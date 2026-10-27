# Self-hosting

One binary runs the whole farm: the collar endpoints, the API, the live feed, the web UI, alerts
and texting. Everything in this guide is in the free app.

## Where to run it

A machine that stays on: a farm PC, a Raspberry Pi 4 or 5 (64-bit OS), a small VPS, or a Mac. At
250 collars the server uses about 200 MB of memory. Collars reach it over the internet, so it
needs an address they can reach (below).

## Start it

```
mkdir openpasture && cd openpasture
tar xzf ../openpasture-server-linux-arm64.tar.gz
./openpasture serve --bind 0.0.0.0 --port 7878
```

`serve` prints a sign-in URL for this machine (`http://127.0.0.1:7878/#/signin/…`): open it to
sign in as the owner. It works once, within 10 minutes; `./openpasture sign-in` prints another for
the running server. Being on the machine grants nothing by itself. From anywhere else the app asks
for the app token once:

```
./openpasture token
```

Then add people in Settings > People. Each gets their own sign-in link and role (viewer, hand,
manager, owner). Someone who only texts needs no sign-in, only a phone.

`openpasture token` reads the token without disturbing a running server.

One server runs per data directory. The server takes a lock on `openpasture.lock` in the data
directory for as long as it runs and writes `server.json` beside it (its pid, version and URL). A
second `openpasture serve` on the same directory stops with "openpasture is already running for …
(pid …, http://…)" instead of starting a second copy of every background job against the same
database. Open the running one, or stop it first. The desktop app opens the running one in its
window instead of starting its own. `server.json` is removed
on a clean stop; one left by a crash is ignored, because nothing holds the lock.

`Ctrl-C` and `SIGTERM` both stop the server cleanly: it finishes open requests (up to 5 s), lets
the background jobs stop (up to 10 s), closes the database and removes `server.json`.

Docker:

```
docker run -d --name openpasture --restart unless-stopped -p 127.0.0.1:7878:7878 -v openpasture:/data ghcr.io/open-pasture/openpasture
docker exec openpasture openpasture token
```

The image runs the server under `tini`, so `docker stop` reaches it as `SIGTERM`, and has a
`HEALTHCHECK` (`openpasture health`, below). Publish the port on `127.0.0.1` and put a proxy with
TLS in front of it (below); `-p 7878:7878` serves plain HTTP on every interface of the host. If you
build your own image without an init, run it with `docker run --init`.

## Keep it running (Linux)

`/etc/systemd/system/openpasture.service`:

```
[Unit]
Description=openpasture
After=network-online.target
Wants=network-online.target

[Service]
User=openpasture
Environment=OPENPASTURE_DATA_DIR=/var/lib/openpasture
ExecStart=/opt/openpasture/openpasture serve --bind 127.0.0.1 --port 7878
Restart=always
# SIGTERM stops it cleanly within about 15 s.
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

```
sudo useradd --system --home /var/lib/openpasture --create-home openpasture
sudo systemctl enable --now openpasture
sudo -u openpasture OPENPASTURE_DATA_DIR=/var/lib/openpasture /opt/openpasture/openpasture token
```

## A public https address

Collars, Twilio's webhook, Web Push and installing the app on a phone all need an https address.
Set it in Settings > Server > Public URL once you have one, and as the collar address (below).
Pick one way that keeps the same name for good:

- **Reverse proxy** on a machine with a public IP and a domain. Caddy gets the certificate itself:

  ```
  farm.example.com {
      reverse_proxy 127.0.0.1:7878
  }
  ```

  nginx, with a certificate from certbot. Pass the client's address on: per-address limits (sign-in
  tries, wrong tokens, collars' refusals) count it, and without it every visitor counts as the
  proxy. Keep the WebSocket upgrade for the live feed:

  ```
  location / {
      proxy_pass http://127.0.0.1:7878;
      proxy_set_header Host $host;
      proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
      proxy_set_header X-Forwarded-Proto $scheme;
      proxy_http_version 1.1;
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_read_timeout 120s;
  }
  ```

- **Cloudflare named tunnel** from behind NAT, on your own domain
  (`cloudflared tunnel create farm`, a DNS route to it, then `cloudflared tunnel run farm` with
  `service: http://127.0.0.1:7878`). Not a quick tunnel (`cloudflared tunnel --url …`): its
  `trycloudflare.com` name changes every time it starts, and collars keep the name they were given.
- **Tailscale Funnel**: `tailscale funnel 7878` (the `*.ts.net` name stays with the machine).

The desktop app works the same way: it keeps the server on `127.0.0.1` and the tunnel reaches it
there. It stays on its port (7878 unless changed) whenever a public URL or collar address is set,
so the tunnel keeps pointing at it; if another program takes the port it says so instead of
moving. Closing its window leaves the server running; quit it from the menu.

Every request to `/api` and `/mcp` needs a token, wherever it comes from, so a proxy or tunnel on
the same machine (which makes every visitor look local) gives nobody the owner. Keep the server
bound to `127.0.0.1` when a proxy or tunnel is in front of it.

### The collar address

What collars are given is set on its own, in Settings > Server > Collar address, and only after a
check: the name resolves, https answers with a certificate a collar accepts, and what answers is
this server (it asks itself a one-time question through the address). Then it asks you to confirm,
because the address is written into every collar: on its QR card, and in the signed config of
collars that take configs. A collar given an address that stops answering keeps trying it for up
to a day before it falls back, enforcing its fence meanwhile but unheard. Until a collar address
is set, Add collar and bulk linking hand out keys without an endpoint and say why, and cards
can't be printed. This machine's own address (`127.0.0.1`, `localhost`) is never handed out.

Without a public URL the app still works on the farm's network: texts in arrive by polling
Twilio and browsers use a bookmark instead of the installed app. Collar firmware talks only to
https, so a collar on the bench reaches a server without one through the serial bridge
(`tools/collar-bridge`), which talks to `http://127.0.0.1:7878` for it.

## Collars

Link collars from the Herd view (Link collars takes a CSV of `tag,collar name`) and print their QR
cards. A collar is provisioned from its card over USB serial: a handheld scanner types the card
into a serial terminal (`provision {…}`), so the card is all a collar needs. `collar-sim` (in the tarball), the simulator, speaks the same protocol
for a trial without hardware, on a throwaway farm of its own (port 7991, a temporary data dir):

```
./collar-sim farm
```

Never point it at this farm: it refuses port 7878 and any data dir with real collars, and the
app marks simulated collars "Simulated" (`simulation/README.md`).

## Health and logs

`GET /healthz` needs no sign-in and says nothing private, so a load balancer, Docker or an uptime
monitor can probe it:

```
{"status":"ok","version":"0.2.2","db":"ok","jobs":[{"name":"alerts.engine","status":"running","restarts":0}, …]}
```

- `ok` (200): the database answers and every background job is running on time.
- `degraded` (200): a job is restarting after a crash, or hasn't reported in for several of its
  intervals. The server still answers; Settings > Server (`GET /api/server`, `tasks`) shows which
  job, its last error and how often it restarted.
- `down` (503): the database doesn't answer, or the server is stopping.

Every background job (the alert engine, the move, escape and schedule drivers, the boundary
activation watcher, the decision scheduler, outgoing texts, the telemetry rollup, the WAL
checkpointer) runs under a supervisor: one that panics or stops on its own is logged at `error`
and started again after 1 s, doubling to at most a minute.

`openpasture health` asks the server named in `server.json` (or `--url http://…`) and exits
non-zero unless it answers 200; the Docker image uses it as its `HEALTHCHECK`.

Logs go to stdout (`journalctl -u openpasture`, `docker logs`). Set `OPENPASTURE_LOG_DIR` to write
them to `<dir>/server.log` instead, rotated at 10 MB with 5 old files kept (`server.log.1` is the
newest). The desktop app logs to `<data dir>/logs/desktop.log`, rotated the same way. `RUST_LOG`
sets the level (default `info`). Each request is one `info` line with target `request`: method,
path, status, milliseconds and peer, with `token`, `key`, `code`, `secret` and `sig` query values
replaced by `REDACTED`. At 250 collars that is about 360,000 lines a day; to keep only errors from
requests use `RUST_LOG=info,request=warn`.

A request that fails inside the server gets `{"error": "Something went wrong on the server (ref
7Q2M9X4KJD). …"}`; the details are in the log next to `log_ref=7Q2M9X4KJD`.

## Texting and alerts

See `docs/TEXTING.md`: your own Twilio number (texts and approvals), email over your own SMTP, a
signed webhook, or the hosted relay.

## Data and backups

Everything lives in the data directory: `openpasture.db` (SQLite in WAL mode), the Parquet day
files of older telemetry (`telemetry/`), `secrets.json` (API keys, 0600), `server_ed25519.key`
(the key collars check boundaries against), `backups/` (snapshots taken before upgrades), `logs/`,
and `openpasture.lock` and `server.json` while a server runs. Lose the server key and every collar
has to be provisioned again, so back it up with the rest.

`secrets.json` and `server_ed25519.key` are in plain text. Keep backups encrypted (restic, borg,
`age`, or an encrypted disk): anyone with the key can sign boundaries your collars will apply.

`--data-dir DIR` or `OPENPASTURE_DATA_DIR` picks the directory; otherwise it is
`~/.local/share/openpasture` on Linux and `~/Library/Application Support/openpasture` on macOS.

A consistent copy while the server runs:

```
sqlite3 /var/lib/openpasture/openpasture.db ".backup /backup/openpasture.db"
rsync -a --exclude 'openpasture.db*' /var/lib/openpasture/ /backup/
```

Or stop the server and copy the whole directory. `GET /api/server` reports what the directory
uses (`disk`: database, WAL, telemetry, snapshots, logs, total and the disk's free space).

## Upgrades

Replace the binary (or pull the new image) and start it again. Migrations run at start and never
drop your data. When a release brings new migrations, the server first writes a consistent copy
of the database (`VACUUM INTO`) to `<data dir>/backups/pre-migrate-<time>-v<last migration>.db`
and keeps the newest 3. If it can't (a full disk), it stops without migrating.

An older binary can't open a database a newer one migrated. It stops with "was last opened by a
newer openpasture" and names the snapshot folder. To go back a version: stop the server, move
`openpasture.db` aside, delete `openpasture.db-wal` and `openpasture.db-shm` if present, copy the
snapshot from before the upgrade to `openpasture.db`, and start the older binary. Anything
recorded since the upgrade is only in the database you moved aside.

## Settings that can't be read

The server won't start if its stored settings can't be read (a hand edit, or a newer release
wrote a value this one doesn't know). It never replaces them with defaults, because that would
issue a new app token and sign out every phone and integration. With the server stopped:

```
openpasture settings repair
```

keeps every setting that can be read, the app token among them, resets the rest to defaults, and
keeps the unreadable copy in the database under `settings.unreadable.<time>`.

## Raw telemetry retention

Fixes, cues and health readings stay in SQLite for 3 days, then move to daily Parquet files, and
are kept for good by default. `PUT /api/server/retention` (owner) sets how many days of each to
keep, at least 7; `null` keeps everything:

```
{"fixes_days": 400, "cues_days": null, "health_days": 90}
```

The hourly rollup deletes whole days older than that. Day summaries (paddock dwell, coverage,
battery, welfare marks) and episodes are kept regardless. The welfare record and grazing reports
read raw cues and fixes, so they can only cover the days you keep.

## Tuning

`OPENPASTURE_DB_POOL` sets the SQLite connection pool (1-256, default 16). The defaults are sized
for 250 collars reporting every 60 s, and every 10 s while a herd is being moved.
