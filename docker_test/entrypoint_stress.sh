#!/bin/bash
set -e
set -o pipefail

LOG_DIR="/app/results"
COORD_DIR="/coord"
mkdir -p "$LOG_DIR"

collect_logs() {
    local exit_code=$?
    local ts
    ts=$(date +%Y%m%d_%H%M%S)
    local out_dir="$LOG_DIR/$ts"
    mkdir -p "$out_dir"

    for f in "$LOG_DIR/iroh.log" "$LOG_DIR/iroh_2.log" "$LOG_DIR/game_server.log"; do
        if [ -f "$f" ]; then
            cp -f "$f" "$out_dir/"
        fi
    done

    if [ -f "$LOG_DIR/game_client.log" ]; then
        cp -f "$LOG_DIR/game_client.log" "$out_dir/"
    fi

    rm -f "$COORD_DIR"/* 2>/dev/null || true
    echo "Saved logs to $out_dir (exit code: $exit_code)"
}

trap collect_logs EXIT

GAME_TEST_DURATION=${GAME_TEST_DURATION:-10}

NETEM_DELAY_MS=${NETEM_DELAY_MS:-200}
NETEM_JITTER_MS=${NETEM_JITTER_MS:-50}
NETEM_LOSS_PCT=${NETEM_LOSS_PCT:-5}
NETEM_REORDER_PCT=${NETEM_REORDER_PCT:-2}
NETEM_REORDER_GAP=${NETEM_REORDER_GAP:-1}
ENABLE_IPERF_NETEM=${ENABLE_IPERF_NETEM:-1}

GAME_NETEM_DELAY_MS=${GAME_NETEM_DELAY_MS:-120}
GAME_NETEM_JITTER_MS=${GAME_NETEM_JITTER_MS:-40}
GAME_NETEM_LOSS_PCT=${GAME_NETEM_LOSS_PCT:-2}
GAME_NETEM_REORDER_PCT=${GAME_NETEM_REORDER_PCT:-5}
GAME_NETEM_REORDER_GAP=${GAME_NETEM_REORDER_GAP:-25}
TOPIC="${TOPIC:-test_topic_default}"

tc qdisc add dev eth0 root netem delay 300ms 100ms distribution normal loss 15% 25% reorder 5% 50% duplicate 1% corrupt 0.1% rate 1mbit
#tc qdisc add dev eth0 root netem delay 200ms 50ms loss 2%

signal_ready() {
    local signal_name="$1"
    touch "$COORD_DIR/$signal_name"
    echo "[COORD] Signaled: $signal_name"
}

wait_for_signal() {
    local signal_name="$1"
    local timeout_sec="${2:-120}"
    local start_time
    start_time=$(date +%s)

    echo "[COORD] Waiting for signal: $signal_name (timeout: ${timeout_sec}s)"
    while [ $(( $(date +%s) - start_time )) -lt $timeout_sec ]; do
        if [ -f "$COORD_DIR/$signal_name" ]; then
            echo "[COORD] Received signal: $signal_name"
            return 0
        fi
        sleep 0.5
    done

    echo "[COORD] TIMEOUT waiting for: $signal_name"
    return 1
}

run_iperf_once() {
    local host="$1"
    iperf3 -c "$host" -t 60 -l 4000 --connect-timeout 30000
}

clear_signal() {
    local signal_name="$1"
    rm -f "$COORD_DIR/$signal_name"
}

echo "Starting iroh-lan..."
if [ "${STRESS_VERBOSE:-}" = "1" ]; then
    export RUST_LOG="${RUST_LOG:-iroh_lan=trace,iroh=info,iroh_gossip=info}"
    export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
else
    export RUST_LOG="${RUST_LOG:-info}"
fi

rm -f "$COORD_DIR"/* 2>/dev/null || true

: > "$LOG_DIR/iroh.log"
/app/bin/iroh-lan "$TOPIC" -t >> "$LOG_DIR/iroh.log" 2>&1 &
IROH_PID=$!

echo "Waiting for IP assignment (AssignedIp)..."
TIMEOUT=120
start_time=$(date +%s)

GOT_IP=""
while [ $(( $(date +%s) - start_time )) -lt $TIMEOUT ]; do
    if grep -q "AssignedIp(" "$LOG_DIR/iroh.log"; then
        GOT_IP=$(grep "AssignedIp(" "$LOG_DIR/iroh.log" | tail -n1 | sed -n 's/.*AssignedIp(\([0-9.]*\)).*/\1/p')
        break
    fi
    sleep 1
done

if [ -z "$GOT_IP" ]; then
    echo "Timeout waiting for AssignedIp."
    cat "$LOG_DIR/iroh.log"
    exit 1
fi

echo "Assigned IP: $GOT_IP"
if [ "$GOT_IP" == "172.30.0.2" ]; then
    ROLE="server"
    PEER_IP="172.30.0.3"
    echo "I am the SERVER (since I got .2)"
elif [ "$GOT_IP" == "172.30.0.3" ]; then
    ROLE="client"
    PEER_IP="172.30.0.2"
    echo "I am the CLIENT (since I got .3)"
else
    echo "Error: Unexpected IP $GOT_IP"
    exit 1
fi

echo "Waiting for interface with IP $GOT_IP..."
TUN_WAIT_START=$(date +%s)
TUN_WAIT_TIMEOUT=120
TUN_DEV=""
TUN_LOGGED=0
while [ $(( $(date +%s) - TUN_WAIT_START )) -lt $TUN_WAIT_TIMEOUT ]; do
    if grep -q "TunActor started for IP: $GOT_IP" "$LOG_DIR/iroh.log"; then
        TUN_LOGGED=1
    fi
    TUN_DEV=$(ip -o -4 addr show | awk -v ip="$GOT_IP" '$4 ~ ip"/" {print $2; exit}')
    if [ -n "$TUN_DEV" ]; then
        break
    fi
    if [ "$TUN_LOGGED" -eq 1 ]; then
        TUN_DEV=$(ip -o link show type tun | awk -F': ' 'NR==1{print $2; exit}')
        if [ -n "$TUN_DEV" ]; then
            break
        fi
    fi
    sleep 0.5
done

if [ -z "$TUN_DEV" ]; then
    echo "Timeout waiting for interface with IP $GOT_IP."
    echo "Last 200 lines of iroh.log:"
    tail -n 200 "$LOG_DIR/iroh.log" || true
    ip -o -4 addr show || true
    exit 1
fi

tc qdisc del dev eth0 root 2>/dev/null || true
NETEM_DEV="${NETEM_DEV:-$TUN_DEV}"
echo "Using network device: $NETEM_DEV for netem"

if [ "$ROLE" == "server" ]; then
    echo "Starting Game Check Server..."
    /app/bin/examples/game_check server "0.0.0.0:30000" "none" "$GAME_TEST_DURATION" 2>&1 | tee -a "$LOG_DIR/game_server.log" &
    GAME_SERVER_PID=$!

    echo "Starting iperf3 server (persistent)..."
    iperf3 -s --idle-timeout 60 2>&1 | sed 's/^/[iperf3] /' &
    IPERF_PID=$!
    for i in {1..40}; do
        if ss -ltn | awk '{print $4}' | grep -q ":5201$"; then
            signal_ready "iperf_ready"
            break
        fi
        sleep 0.5
    done

    signal_ready "server_ready"

    wait_for_signal "tests_complete" 45000  # 12.5 hour timeout for long game test

    echo "Tests complete, shutting down..."
    kill $IPERF_PID 2>/dev/null || true
    kill $GAME_SERVER_PID 2>/dev/null || true
    kill $IROH_PID 2>/dev/null || true

    echo "SERVER DONE."
    exit 0

elif [ "$ROLE" == "client" ]; then

    wait_for_signal "server_ready" 120

    echo "TEST 1/5: Broadcast Hostility Test"
    ping -b -i 0.2 -c 100 255.255.255.255 > /dev/null 2>&1 &
    PING_PID=$!

    echo "TEST 2/5: Throughput (clean network) 120s"
    tc qdisc del dev eth0 root 2>/dev/null || true
    run_iperf_once "$PEER_IP"

    echo "TEST 3/5: Throughput (degraded network) 120s"
    if [ "$ENABLE_IPERF_NETEM" = "1" ]; then
        tc qdisc add dev "$NETEM_DEV" root netem \
            delay ${NETEM_DELAY_MS}ms ${NETEM_JITTER_MS}ms distribution normal \
            loss ${NETEM_LOSS_PCT}% \
            reorder ${NETEM_REORDER_PCT}% ${NETEM_REORDER_GAP}
        sleep 1
    fi

    run_iperf_once "$PEER_IP"

    tc qdisc del dev "$NETEM_DEV" root 2>/dev/null || true
    tc qdisc add dev eth0 root netem delay 300ms 100ms distribution normal loss 15% 25% reorder 5% 50% duplicate 1% corrupt 0.1% rate 1mbit

    echo "TEST 4/5: Reconnection Event"
    echo "Killing iroh-lan..."
    kill $IROH_PID 2>/dev/null || true
    wait $IROH_PID 2>/dev/null || true
    sleep 2

    echo "Restarting iroh-lan..."
    : > "$LOG_DIR/iroh_2.log"
    /app/bin/iroh-lan "$TOPIC" -t >> "$LOG_DIR/iroh_2.log" 2>&1 &
    IROH_PID_2=$!

    echo "Waiting for new IP assignment..."
    NEW_IP=""
    TIMEOUT=120
    for i in $(seq 1 $TIMEOUT); do
        if grep -q "My IP is" "$LOG_DIR/iroh_2.log"; then
            NEW_IP=$(grep "My IP is" "$LOG_DIR/iroh_2.log" | tail -n1 | awk '{print $NF}')
            echo "New IP: $NEW_IP"
            break
        fi
        sleep 1
    done

    if [ -z "$NEW_IP" ]; then
        echo "Failed to get new IP after restart."
        cat "$LOG_DIR/iroh_2.log"
        exit 1
    fi

    echo "Waiting for tun device after reconnection..."
    for i in $(seq 1 $TIMEOUT); do
        if ls /sys/class/net/ | grep -q tun; then
            TUN_DEV=$(ls /sys/class/net/ | grep '^tun' | head -n1)
            NETEM_DEV="$TUN_DEV"
            break
        fi
        sleep 1
    done

    echo "TEST 5/5: Game simulation (${GAME_TEST_DURATION}m)"
    echo "Network: ${GAME_NETEM_DELAY_MS}ms delay, ${GAME_NETEM_LOSS_PCT}% loss"

    echo "Running game client..."
    /app/bin/examples/game_check client "0.0.0.0:0" "$PEER_IP:30000" "$GAME_TEST_DURATION" 2>&1 | tee -a "$LOG_DIR/game_client.log"
    GAME_EXIT=${PIPESTATUS[0]}

    tc qdisc del dev "$NETEM_DEV" root 2>/dev/null || true

    kill $PING_PID 2>/dev/null || true
    kill $IROH_PID_2 2>/dev/null || true

    signal_ready "tests_complete"

    if [ $GAME_EXIT -eq 0 ]; then
        echo "ALL STRESS TESTS PASSED"
        exit 0
    else
        echo "GAME SIMULATION FAILED."
        exit 1
    fi
fi
