#!/usr/bin/env bash
# Timed hotpath profile of datavis under synthetic WebSocket load.
#
# Launches the app with hotpath instrumentation, feeds it Influx line-protocol
# frames via the ws_influx_load example, measures for a fixed number of seconds,
# then lets the app self-exit (DATAVIS_EXIT_AFTER_S) so hotpath flushes a JSON
# report. Prints the report path on success.
#
# All knobs are env-overridable, e.g.:  DURATION=120 FREQ=50000 ./profile.sh
set -euo pipefail

# Repo root, from .claude/skills/profiling/
cd "$(dirname "$0")/../../.."

# ---- knobs ----
DURATION="${DURATION:-60}"                     # seconds to measure (app self-exits)
FEATURES="${FEATURES:-hotpath,hotpath-alloc}"  # hotpath | hotpath-alloc | hotpath-cpu
NO_DEFAULT="${NO_DEFAULT:-1}"                  # 1 => --no-default-features (drop pyo3/Python threads)
OUT="${OUT:-$(pwd)/hotpath-$(date +%Y%m%d-%H%M%S).json}"
WS_PORT="${WS_PORT:-9002}"                     # config.toml's [[sources.websocket]] listen port
# load generator (defaults reproduce the reference profile)
CHANNELS="${CHANNELS:-8}"
FREQ="${FREQ:-100000}"
STATE_CHANNELS="${STATE_CHANNELS:-4}"
STATES="${STATES:-aaaa,bbbbb,cccc}"
STATE_DWELL="${STATE_DWELL:-2}"
MEASUREMENT="${MEASUREMENT:-load}"

ndf=""; [ "$NO_DEFAULT" = "1" ] && ndf="--no-default-features"

echo "[profile] building app ($ndf --release --features $FEATURES) + load generator ..."
cargo build $ndf --release --features "$FEATURES" >/dev/null
cargo build --release --example ws_influx_load >/dev/null

# ---- display: reuse $DISPLAY if set, else headless Xvfb :99 (software GL) ----
XVFB_PID=""
if [ -z "${DISPLAY:-}" ]; then
  echo "[profile] no \$DISPLAY; starting Xvfb :99 with software GL"
  Xvfb :99 -screen 0 1600x1000x24 >/dev/null 2>&1 &
  XVFB_PID=$!
  export DISPLAY=:99
  export LIBGL_ALWAYS_SOFTWARE=1
  sleep 1
fi
cleanup() { [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true; }
trap cleanup EXIT

rm -f "$OUT"

echo "[profile] launching datavis: measure ${DURATION}s, report -> $OUT"
DATAVIS_EXIT_AFTER_S="$DURATION" \
HOTPATH_OUTPUT_FORMAT=json-pretty \
HOTPATH_OUTPUT_PATH="$OUT" \
  cargo run $ndf --release --features "$FEATURES" >/tmp/datavis-profile.log 2>&1 &
APP_PID=$!

# ---- wait for the ws server to accept a TCP connection ----
echo "[profile] waiting for ws://127.0.0.1:$WS_PORT ..."
up=0
for _ in $(seq 1 150); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$WS_PORT") 2>/dev/null; then exec 3>&- 3<&-; up=1; break; fi
  kill -0 "$APP_PID" 2>/dev/null || { echo "[profile] app exited early — see /tmp/datavis-profile.log"; exit 1; }
  sleep 0.2
done
[ "$up" = 1 ] || { echo "[profile] ws server never came up — see /tmp/datavis-profile.log"; kill "$APP_PID" 2>/dev/null || true; exit 1; }

echo "[profile] feeding: $CHANNELS ch @ ${FREQ}Hz, $STATE_CHANNELS state ch ($STATES) for ${DURATION}s"
cargo run --release --example ws_influx_load -- \
  --url "ws://127.0.0.1:$WS_PORT" \
  --channels "$CHANNELS" --freq "$FREQ" --measurement "$MEASUREMENT" \
  --state-channels "$STATE_CHANNELS" --states "$STATES" --state-dwell "$STATE_DWELL" \
  --duration "$DURATION" >/tmp/ws_influx_load.log 2>&1 &
FEED_PID=$!

# ---- app self-exits at the deadline, which flushes the report ----
wait "$APP_PID" || true
kill "$FEED_PID" 2>/dev/null || true

if [ -s "$OUT" ]; then
  echo "[profile] done. report: $OUT"
else
  echo "[profile] WARNING: no report at $OUT — app may not have exited cleanly."
  echo "[profile] app log: /tmp/datavis-profile.log ; feeder log: /tmp/ws_influx_load.log"
  exit 1
fi
