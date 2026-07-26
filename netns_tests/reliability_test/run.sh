#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."       # repo root

TOPIC="${TOPIC:-reliability_$RANDOM}"
RUN_ID="${RUN_ID:-$(date +%Y%m%d_%H%M%S)}"
COORD_DIR="$(mktemp -d /tmp/iroh-coord.XXXX)"
RESULTS_ROOT="$PWD/netns_tests/reliability_test/results"
BIN_DIR="$PWD/target/debug"
#RELAY_URL="http://203.0.113.1:3340"
RELAY_URL="${RELAY_URL:-}"

PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    ./netns_tests/netns_lab.sh teardown
    rm -rf "$COORD_DIR"
}
trap cleanup EXIT INT TERM

sudo killall iroh-lan 2>/dev/null || true
./netns_tests/netns_lab.sh setup

# relay in the ISP namespace
ip netns exec il-isp "$HOME/.cargo/bin/iroh-relay" \
    --dev > "$COORD_DIR/relay.log" 2>&1 &
PIDS+=($!)
sleep 2

for i in 0 1 2 3 4; do
    NODE_RESULTS_DIR="$RESULTS_ROOT/node$i"
    NODE_LOG_DIR="$NODE_RESULTS_DIR/$RUN_ID"
    mkdir -p "$NODE_LOG_DIR"

    ip netns exec "il-node$i" env \
        NODE_INDEX=$i NODE_COUNT=5 TOPIC="$TOPIC" RUN_ID="$RUN_ID" \
        RECONNECT_MAX_SEC="${RECONNECT_MAX_SEC:-60}" \
        MESH_TICK_MS="${MESH_TICK_MS:-100}" \
        RUST_LOG="${RUST_LOG:-iroh_lan=trace,iroh=info,iroh_gossip=info}" \
        COORD_DIR="$COORD_DIR" \
        RESULTS_DIR="$NODE_RESULTS_DIR" \
        BIN_DIR="$BIN_DIR" \
        ${RELAY_URL:+IROH_RELAY_URL="$RELAY_URL"} \
        RUST_BACKTRACE=full \
        bash netns_tests/reliability_test/entrypoint.sh \
        > "$NODE_LOG_DIR/entrypoint.log" 2>&1 &
    PIDS+=($!)
done

fail=0
for pid in "${PIDS[@]:1}"; do    # skip relay pid
    wait "$pid" || fail=1
done
kill "${PIDS[0]}" 2>/dev/null || true
exit $fail