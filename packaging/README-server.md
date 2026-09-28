# openpasture server

`openpasture` runs collars, stores their data, analyses it, and decides where the herd goes.
The web UI is built in. `collar-sim` runs simulated collars against it.

```
./openpasture serve                              # http://127.0.0.1:7878
./openpasture serve --bind 0.0.0.0 --port 7878   # reachable from other machines
```

Off this machine the app asks for its token:

```
./openpasture token
```

Data lives in `OPENPASTURE_DATA_DIR`, or the platform data directory
(`~/.local/share/openpasture` on Linux, `~/Library/Application Support/openpasture` on macOS).
Set it with `--data-dir DIR` on both commands to use another one.

On macOS, a downloaded binary is quarantined. Clear it once:

```
xattr -d com.apple.quarantine openpasture collar-sim
```

## Simulated collars

Create a farm, a paddock and a herd in the app, then:

```
./collar-sim --server http://127.0.0.1:7878 --count 12
```

Add `--token <app token>` when the server is on another machine.

## For a farm

Collars and phones need an https address (a reverse proxy, Cloudflare Tunnel or Tailscale
Funnel); set it in Settings > Server > Public URL. Back up the data directory, including
`server_ed25519.key`: collars trust that key.

Texts, alerts and approvals work with your own Twilio number (Settings > Texting). Replies come in
by Twilio's webhook to `<public URL>/hooks/twilio/sms`, or, with no public URL, by polling Twilio
every 10 s.

Guides: https://github.com/open-pasture/openpasture/blob/main/docs/SELF-HOSTING.md and
https://github.com/open-pasture/openpasture/blob/main/docs/TEXTING.md

Source, docs and issues: https://github.com/open-pasture/openpasture

Licence: AGPL-3.0 (see LICENSE).
