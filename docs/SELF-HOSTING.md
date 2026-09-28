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

Open `http://<host>:7878`. On the machine itself no token is needed. From anywhere else the app
asks for the app token once:

```
./openpasture token
```

Then add people in Settings > People. Each gets their own sign-in link and role (viewer, hand,
manager, owner). Someone who only texts needs no sign-in, only a phone.

Docker:

```
docker run -d --name openpasture --restart unless-stopped -p 7878:7878 -v openpasture:/data ghcr.io/open-pasture/openpasture
docker exec openpasture openpasture token
```

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
Set it in Settings > Server > Public URL once you have one. Pick one way:

- **Reverse proxy** on a machine with a public IP and a domain. Caddy gets the certificate itself:

  ```
  farm.example.com {
      reverse_proxy 127.0.0.1:7878
  }
  ```

- **Cloudflare Tunnel** from behind NAT: `cloudflared tunnel --url http://127.0.0.1:7878`, or a
  named tunnel for a stable address.
- **Tailscale Funnel**: `tailscale funnel 7878`.

Requests through a proxy or tunnel are never treated as local, so they always need a token. Keep
the server bound to `127.0.0.1` when a proxy or tunnel is in front of it.

The public URL is written into collars when they are provisioned (the QR cards need an https
URL), so pick one that will last. A later change reaches collars with the `config` capability in
their next signed config.

Without a public URL the app still works on the farm's network: texts in arrive by polling
Twilio, browsers use a bookmark instead of the installed app, and a collar on the bench can
report to `http://<host>:7878/collar/v1`.

## Collars

Link collars from the Herd view (Link collars takes a CSV of `tag,collar name`) and print their QR
cards. A collar is provisioned from its card over USB serial: a handheld scanner types the card
into a serial terminal (`provision {…}`), so the card is all a collar needs. `collar-sim` (in the tarball) speaks the same protocol
for a trial without hardware:

```
./collar-sim --server http://127.0.0.1:7878 --count 12
```

## Texting and alerts

See `docs/TEXTING.md`: your own Twilio number (texts and approvals), email over your own SMTP, a
signed webhook, or the hosted relay.

## Data and backups

Everything lives in the data directory: `openpasture.db` (SQLite in WAL mode), the Parquet day
files of older telemetry, `secrets.json` (API keys, 0600) and `server_ed25519.key`, the key
collars check boundaries against. Lose that key and every collar has to be provisioned again, so
back it up with the rest.

`--data-dir DIR` or `OPENPASTURE_DATA_DIR` picks the directory; otherwise it is
`~/.local/share/openpasture` on Linux and `~/Library/Application Support/openpasture` on macOS.

A consistent copy while the server runs:

```
sqlite3 /var/lib/openpasture/openpasture.db ".backup /backup/openpasture.db"
rsync -a --exclude 'openpasture.db*' /var/lib/openpasture/ /backup/
```

Or stop the server and copy the whole directory.

## Upgrades

Replace the binary (or pull the new image) and start it again. Migrations run at start and never
drop your data; a newer database can't be opened by an older binary, so back up first.

## Tuning

`OPENPASTURE_DB_POOL` sets the SQLite connection pool (1-256, default 16). The defaults are sized
for 250 collars reporting every 60 s, and every 10 s while a herd is being moved.
