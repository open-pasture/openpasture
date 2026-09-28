#!/bin/zsh
# The 250-collar soak (dev tool, field-ready stream L): one server and
# collar-sim with 250 collars for hours, the way a farm runs.
#
#   scripts/soak.sh [--hours 6] [--port 17150] [--dir /tmp/op-fr/L/soak] [--bin target/release]
#
# - Cows (238, P1) are moved every 30 min between the thirds of P1, about
#   90 m a sweep; each sweep is recorded (scripts/soak/driver.py).
# - Heifers (12, P3) run a strip schedule with a back fence; the next strip
#   opens every hour (move-now) and the back fence strands the heifers still
#   on the old strip: an escape (and its alert) every hour.
# - Alerts go to a farm webhook on port+3 (scripts/soak/hook.py).
# - Collars reach the server through a counting proxy on port+2
#   (scripts/soak/proxy.py): bytes per request kind and per collar, and the
#   server's latency for each request.
# - One browser stays connected on the Herd table, with the map every hour
#   (scripts/soak/ui.ts; relaunched if it dies), a socket counts /api/live
#   frames (scripts/soak/ws.mjs), the driver samples CPU, RSS, the macOS
#   memory footprint and the data dir every 30 s.
# MOVE_EVERY and ADVANCE_EVERY (minutes) change the two cadences for a short trial run.
# At the end everything is stopped and scripts/soak/report.py writes
# <dir>/report.md. Needs a release build: cargo build --release -p op-cli -p collar-sim.
set -e
HOURS=6; PORT=17150; DIR=/tmp/op-fr/L/soak; BIN=
ROOT=$(cd "$(dirname "$0")/.." && pwd)
while [ $# -gt 0 ]; do
  case $1 in
    --hours) HOURS=$2; shift 2;;
    --port) PORT=$2; shift 2;;
    --dir) DIR=$2; shift 2;;
    --bin) BIN=$2; shift 2;;
    *) echo "unknown option $1"; exit 2;;
  esac
done
BIN=${BIN:-$ROOT/target/release}
S=$ROOT/scripts/soak
U=http://127.0.0.1:$PORT
SECS=$(python3 -c "print(int($HOURS * 3600))")
[ -x $BIN/openpasture ] && [ -x $BIN/collar-sim ] || { echo "no release build in $BIN"; exit 1; }
[ -d $ROOT/scripts/shotdiff/node_modules ] || (cd $ROOT/scripts/shotdiff && bun install > /dev/null)

rm -rf $DIR && mkdir -p $DIR
PIDS=()
stop() {
  for p in ${(Oa)PIDS}; do kill $p 2>/dev/null || true; done
  sleep 2
  for p in ${(Oa)PIDS}; do kill -9 $p 2>/dev/null || true; done
}
trap stop EXIT
trap 'stop; exit 130' INT TERM

RUST_LOG=info $BIN/openpasture serve --port $PORT --data-dir $DIR/data > $DIR/server.log 2>&1 & SERVER=$!; PIDS+=($SERVER)
python3 $S/proxy.py --listen $((PORT+2)) --upstream 127.0.0.1:$PORT --out $DIR/proxy.jsonl --every 60 > $DIR/proxy.log 2>&1 & PIDS+=($!)
python3 $S/hook.py --port $((PORT+3)) --out $DIR/hook.jsonl > $DIR/hook.log 2>&1 & PIDS+=($!)
(cd $S && python3 -c "
import farm
api = farm.Api('$U'); farm.wait_up(api)
farm.setup(api, [('Cows', 238, 'P1'), ('Heifers', 12, 'P3')], public_url='http://127.0.0.1:$((PORT+2))')
api.put('/api/notify/channels', {'webhook': {'url': 'http://127.0.0.1:$((PORT+3))/hook'}, 'secrets': {'webhook_secret': 'soak-webhook-secret'}})
")
echo "$(date -u +%FT%TZ) soak for $HOURS h on $U, data in $DIR"
$BIN/collar-sim --server $U --herds Cows=238,Heifers=12 --state $DIR/sim.json --ramp 60 --duration $((SECS + 120)) > $DIR/sim.log 2>&1 & PIDS+=($!)
node $S/ws.mjs ws://127.0.0.1:$PORT/api/live $DIR/ws.jsonl $SECS > $DIR/ws.log 2>&1 & PIDS+=($!)
bun $S/ui.ts $U $DIR/ui $SECS > $DIR/ui.log 2>&1 & PIDS+=($!)
(cd $S && exec python3 driver.py --api $U --dir $DIR --server-pid $SERVER --hours $HOURS --move-every ${MOVE_EVERY:-30} --advance-every ${ADVANCE_EVERY:-60} > $DIR/driver.log 2>&1) & DRIVER=$!; PIDS+=($DRIVER)
# In the background and waited for, so a signal to this script stops everything at once.
wait $DRIVER || true
trap - EXIT INT TERM
stop
python3 $S/report.py $DIR > $DIR/report.md
echo "$(date -u +%FT%TZ) done: $DIR/report.md"
