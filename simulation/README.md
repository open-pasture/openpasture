# Simulation

Everything that pretends to be a farm lives here, apart from the product.

- `collar-sim/`: simulated OpenCollar devices. Each one is linked through the app API like a
  person would, then talks to the real collar endpoints with its own key: fix batches, signed
  boundary downloads, acks, configs. The herd grazes, follows its neighbours, and walks after a
  moving fence.

The rule: **the simulator is never product data and is never pointed at a real farm.**

- Nothing in this directory is compiled into the server or the app. The server tarball carries
  `collar-sim` as a separate binary, described as the simulator.
- Simulated collars say so. They are linked with `simulated: true` and report the `sim` cap in
  `device.caps`, and the server keeps `collars.simulated` for good once either happened. The app
  shows "Simulated" wherever they appear (map, herd list, animal page, status bar), and
  "Simulated collars" in the status bar when every collar on the farm is.
- Simulated collars stay out of official records. Reports (NRCS 528, organic season, lease
  head-days, paddock record, welfare) leave out herds on simulated collars only and animals on
  simulated collars, and their notes say what was left out; a herd with some simulated collars is
  named in the notes. Alerts about simulated collars show in the app and are texted, pushed,
  emailed and briefed to no one.
- The app's first-run story (14 animals over Flint Hills imagery) is a drawing, not this
  simulator. It is labelled "Simulated" on screen for as long as it shows and writes nothing.

## A simulated farm in one command

```sh
cargo build -p op-cli -p collar-sim
target/debug/collar-sim farm
```

`collar-sim farm` starts its own openpasture server on a new data dir under the system temp dir
and port 7991, makes "Simulated farm" in the Flint Hills with two paddocks and a herd, runs 12
collars, and prints the address to open. Ctrl-c stops the collars and the server and deletes the
data dir.

| Flag | |
| --- | --- |
| `--port <n>` | The server's port (default 7991). |
| `--data-dir <dir>` | Keep the farm here; run again with the same dir to pick it up. |
| `--keep` | Keep the new temp data dir. |
| `--openpasture <path>` | The server binary (default: next to `collar-sim`, else on `PATH`). |
| `--count`, `--herds`, `--anchors`, `--caps`, `--imu`, `--cell`, `--drop`, `--ramp`, `--duration` | As for a plain run, below. |
| `--force` | Run anyway on port 7878 or a data dir with real collars. Don't. |

It refuses, unless `--force`:

- port 7878, where a real farm runs;
- the app's own data dir (`OPENPASTURE_DATA_DIR` or the platform data dir);
- a data dir holding any collar not marked simulated (collars from before the flag count as real);
- a non-empty data dir without a `SIMULATED-FARM` file and without simulated collars.

A data dir the farm made carries `SIMULATED-FARM`, the server's log (`server.log`) and the
simulated collars' keys (`collar-sim.json`).

## Against a server you started

```sh
openpasture serve --port 7992 --data-dir /tmp/sim-farm   # a farm made for this, never yours
collar-sim --server http://127.0.0.1:7992 --token "$(openpasture token --data-dir /tmp/sim-farm)" --count 12
```

It refuses port 7878 and a server with any real collar (`--force` overrides). Make a farm and a
herd in the app first; collars and animals tagged from 101 are linked to the first herd (or
`--herd`, or several with `--herds Cows=238,Heifers=12`). Keys live in `~/.collar-sim/state.json`
(or `--state`), so a second run reuses its collars.

## A LoRa herd

`--anchors <n>` makes the first n collars of each herd anchors (LoRa and LTE-M; in protocol v2
they take turns relaying for the herd and broadcast its boundaries) and the rest nodes (LoRa
only). Each still reports to the server itself over protocol v1 for now: anchors report their
LTE-M cell, and every collar's device info carries its device class (`device_class`: `node`,
`anchor`, or `direct` without `--anchors`), which the server records for the herd-network view
(Settings > Advanced > Network). A later change moves nodes onto their anchors over the v2 binding.

## Soak runs

`scripts/soak.sh` runs a release server on a temp data dir and collar-sim with 250 collars for
hours; see the script's header.
