#!/bin/bash
set -e
set -o pipefail

RESULTS_DIR="${RESULTS_DIR:-/app/results}"
RUN_ID="${RUN_ID:-$(date +%Y%m%d_%H%M%S)}"
LOG_DIR="$RESULTS_DIR/$RUN_ID"
QLOG_DIR="$LOG_DIR/qlog.log"
COORD_DIR="${COORD_DIR:-/coord}"
BIN_DIR="${BIN_DIR:-/app/bin}"

mkdir -p "$LOG_DIR" "$QLOG_DIR" "$COORD_DIR"

NODE_INDEX="${NODE_INDEX:-0}"
NODE_COUNT="${NODE_COUNT:-5}"
EXPECTED_PEERS="${EXPECTED_PEERS:-4}"
TOPIC="${TOPIC:-test_topic_default}"
RECONNECT_MAX_SEC="${RECONNECT_MAX_SEC:-60}"
MESH_TICK_MS="${MESH_TICK_MS:-100}"

BASELINE_SEC="${BASELINE_SEC:-35}"
DEGRADED_SEC="${DEGRADED_SEC:-40}"
POST_ROLLING_SEC="${POST_ROLLING_SEC:-45}"
POST_SIM_SEC="${POST_SIM_SEC:-45}"
RECOVERY_SEC="${RECOVERY_SEC:-35}"
POST_OUTAGE_RECOVERY_SEC="${POST_OUTAGE_RECOVERY_SEC:-45}"

ROLLING_SLOT_SEC="${ROLLING_SLOT_SEC:-10}"
SIM_JITTER_MAX_SEC="${SIM_JITTER_MAX_SEC:-5}"
OUTAGE_SEC="${OUTAGE_SEC:-25}"
OUTAGE_RECOVERY_MAX_SEC="${OUTAGE_RECOVERY_MAX_SEC:-30}"


IROH_PID=""
MY_IP=""
TUN_DEV=""
LOGS_COLLECTED=0

# echo the mesh_check durations and max_reconnect to make sure they make sense
echo "Mesh phase durations and settings:"
echo "  baseline: ${BASELINE_SEC}s"
echo "  degraded: ${DEGRADED_SEC}s (with netem profile)"
echo "  post_rolling: ${POST_ROLLING_SEC}s (with netem profile)"
echo "  post_simultaneous: ${POST_SIM_SEC}s (with netem profile)"
echo "  post_outage_recovery: ${POST_OUTAGE_RECOVERY_SEC}s (nonfatal, with clean netem profile)"
echo "  recovery: ${RECOVERY_SEC}s (with clean netem profile)"
echo "  max_reconnect: ${RECONNECT_MAX_SEC}s"
echo "  rolling_slot_sec: ${ROLLING_SLOT_SEC}s"
echo "  sim_jitter_max_sec: ${SIM_JITTER_MAX_SEC}s"
echo "  outage_sec: ${OUTAGE_SEC}s"
echo "  outage_recovery_max_sec: ${OUTAGE_RECOVERY_MAX_SEC}s"
echo "  netem profile for degraded/rolling/simultaneous phases: delay ${NETEM_DELAY_MS}ms ±${NETEM_JITTER_MS}ms, loss ${NETEM_LOSS_PCT}%, reorder ${NETEM_REORDER_PCT}% with gap ${NETEM_REORDER_GAP}, duplicate ${NETEM_DUP_PCT}%"
echo ""


#tc qdisc add dev eth0 root netem delay 300ms 100ms distribution normal loss 1.5% 2.5% reorder 0.5% 5% duplicate 0.1% corrupt 0.01% rate 10mbit
#tc qdisc replace dev eth0 root netem delay 1200ms 400ms reorder 35% 50% loss 35% 10% rate 6mbit limit 4000

collect_logs() {
    local exit_code="${1:-$?}"

    if [ "$LOGS_COLLECTED" -eq 1 ]; then
        return
    fi
    LOGS_COLLECTED=1

    sync "$LOG_DIR" 2>/dev/null || true
    echo "[node${NODE_INDEX}] Saved logs to $LOG_DIR (exit=$exit_code)"
}

finalize_and_exit() {
    local exit_code="$1"

    trap - EXIT TERM INT

    stop_iroh
    tc qdisc del dev eth0 root 2>/dev/null || true
    collect_logs "$exit_code"

    exit "$exit_code"
}

handle_term() {
    local exit_code=143

    if [ -f "$COORD_DIR/TEST_PASS" ]; then
        exit_code=0
    fi

    echo "[node${NODE_INDEX}] received SIGTERM"
    finalize_and_exit "$exit_code"
}

handle_int() {
    echo "[node${NODE_INDEX}] received SIGINT"
    finalize_and_exit 130
}

handle_exit() {
    local exit_code=$?

    trap - EXIT TERM INT
    stop_iroh
    tc qdisc del dev eth0 root 2>/dev/null || true
    collect_logs "$exit_code"
    exit "$exit_code"
}

trap handle_exit EXIT
trap handle_term TERM
trap handle_int INT

signal() {
    local name="$1"
    touch "$COORD_DIR/$name"
    echo "[node${NODE_INDEX}] signal: $name"
}

wait_signal() {
    local name="$1"
    local timeout_sec="${2:-300}"
    local start
    start=$(date +%s)

    echo "[node${NODE_INDEX}] waiting for $name (${timeout_sec}s)"
    while [ $(( $(date +%s) - start )) -lt "$timeout_sec" ]; do
        if [ -f "$COORD_DIR/$name" ]; then
            return 0
        fi
        if [ -f "$COORD_DIR/FAIL" ]; then
            echo "[node${NODE_INDEX}] observed global FAIL"
            exit 1
        fi
        sleep 0.5
    done

    echo "[node${NODE_INDEX}] timeout waiting for $name"
    signal "FAIL"
    touch "$COORD_DIR/FAIL"
    exit 1
}

wait_all_signals() {
    local prefix="$1"
    local timeout_sec="${2:-300}"
    local start
    start=$(date +%s)

    while [ $(( $(date +%s) - start )) -lt "$timeout_sec" ]; do
        local count
        count=$(ls "$COORD_DIR"/${prefix}_* 2>/dev/null | wc -l || true)
        if [ "$count" -ge "$NODE_COUNT" ]; then
            return 0
        fi
        if [ -f "$COORD_DIR/FAIL" ]; then
            echo "[node${NODE_INDEX}] observed global FAIL"
            exit 1
        fi
        sleep 0.5
    done

    echo "[node${NODE_INDEX}] timeout waiting for all ${prefix}_* signals"
    touch "$COORD_DIR/FAIL"
    exit 1
}

start_iroh() {
    local logfile="$1"
    > "$logfile"
    echo "Backtrace will be logged to $logfile"
    export RUST_LOG="${RUST_LOG:-iroh_lan=trace,iroh=info,iroh_gossip=info}"
    export RUST_BACKTRACE=full

    connected_timer=${EPOCHREALTIME/./}
    "$BIN_DIR/iroh-lan" "$TOPIC" -t --qlog-dir "$QLOG_DIR" \
        ${IROH_RELAY_URL:+--relay-url "$IROH_RELAY_URL"} \
        >> "$logfile" 2>&1 &
    IROH_PID=$!
    echo "[node${NODE_INDEX}] started iroh pid=$IROH_PID log=$logfile"
}

stop_iroh() {
    if [ -n "$IROH_PID" ]; then
        kill "$IROH_PID" 2>/dev/null || true
        # Give iroh-lan up to 5s to shut down cleanly, then SIGKILL.
        local _i
        for _i in 1 2 3 4 5; do
            if ! kill -0 "$IROH_PID" 2>/dev/null; then break; fi
            sleep 1
        done
        if kill -0 "$IROH_PID" 2>/dev/null; then
            kill -9 "$IROH_PID" 2>/dev/null || true
            wait "$IROH_PID" 2>/dev/null || true
        fi
        IROH_PID=""
    fi
}

wait_assigned_ip() {
    local logfile="$1"
    local timeout_sec="${2:-180}"
    local start
    start=$(date +%s)

    while [ $(( $(date +%s) - start )) -lt "$timeout_sec" ]; do
        local ip
        ip=$(grep "My IP is " "$logfile" 2>/dev/null | tail -n1 | sed -n 's/My IP is \([0-9.]*\).*/\1/p' || true)
        if [ -n "$ip" ]; then
            MY_IP="$ip"
            echo "[node${NODE_INDEX}] My IP=$MY_IP"
            break
        fi
        sleep 1
    done

    if [ -z "$MY_IP" ]; then
        echo "[node${NODE_INDEX}] timeout waiting My IP"
        touch "$COORD_DIR/FAIL"
        exit 1
    fi

    while [ $(( $(date +%s) - start )) -lt "$timeout_sec" ]; do
        local count
        count=$(grep "Peer connected:" "$logfile" 2>/dev/null | grep -v "Peer connected: $MY_IP " | sed -n 's/Peer connected: \([0-9.]*\).*/\1/p' | sort -u | wc -l || true)
        if [ "$count" -ge "$EXPECTED_PEERS" ]; then
            ms=$(( (${EPOCHREALTIME/./} - connected_timer) / 1000 ))
            echo "[node${NODE_INDEX}] Peer connected count=$count; time-to-all-connected=$(printf '%d,%03d' $((ms/1000)) $((ms%1000)))s"
            return 0
        fi
        sleep 1
    done

    echo "[node${NODE_INDEX}] timeout waiting Peer connected (expected $EXPECTED_PEERS)"
    touch "$COORD_DIR/FAIL"
    exit 1
}

wait_tun_for_ip() {
    local timeout_sec="${1:-120}"
    local start
    start=$(date +%s)

    while [ $(( $(date +%s) - start )) -lt "$timeout_sec" ]; do
        TUN_DEV=$(ip -o -4 addr show | awk -v ip="$MY_IP" '$4 ~ ip"/" {print $2; exit}')
        if [ -n "$TUN_DEV" ]; then
            echo "[node${NODE_INDEX}] TUN_DEV=$TUN_DEV"
            return 0
        fi
        sleep 0.5
    done

    echo "[node${NODE_INDEX}] timeout waiting TUN for $MY_IP"
    ip -o -4 addr show || true
    touch "$COORD_DIR/FAIL"
    exit 1
}

rebuild_peers_list() {
    local out="$COORD_DIR/peers.list"
    ls "$COORD_DIR"/node*.ip 2>/dev/null | sort | while read -r f; do cat "$f"; done | sed '/^$/d' | sort -u > "$out" || true
    echo "[node${NODE_INDEX}] peers.list=$(tr '\n' ',' < "$out")"
}

get_peers_csv() {
    if [ ! -f "$COORD_DIR/peers.list" ]; then
        echo ""
        return
    fi
    grep -v "^$MY_IP$" "$COORD_DIR/peers.list" | paste -sd, - || true
}

#NETEM_DELAY_MS=600
#NETEM_JITTER_MS=200
#NETEM_LOSS="gemodel 5% 15% 100% 0%"
#NETEM_RATE="128kbit"
#NETEM_REORDER_PCT=8
#NETEM_REORDER_CORR=50
#NETEM_DUP_PCT=3
#NETEM_CORRUPT_PCT=0.5
#NETEM_SLOT="100ms 500ms"



apply_netem_profile() {
    local profile="$1"

    tc qdisc del dev eth0 root 2>/dev/null || true

    case "$profile" in
        clean)
            echo "[node${NODE_INDEX}] netem clean"
            ;;
        degraded)
            echo "[node${NODE_INDEX}] netem degraded on eth0"

            # Build the tc command piecewise so each feature
            # can be independently toggled via empty vars
            local cmd="tc qdisc add dev eth0 root netem delay 450ms 80ms distribution normal loss gemodel 1.6% 30% 100% 0% reorder 7% duplicate 3% corrupt 0.1% rate 384kbit"

            echo "[node${NODE_INDEX}] $cmd"
            eval "$cmd"
            ;;
        blackhole)
            echo "[node${NODE_INDEX}] netem blackhole on eth0 (loss 100%)"
            tc qdisc add dev eth0 root netem loss 100%
            ;;
        *)
            echo "[node${NODE_INDEX}] unknown profile '$profile'"
            touch "$COORD_DIR/FAIL"
            exit 1
            ;;
    esac
}

run_outage_cycle() {
    if [ "$NODE_INDEX" = "0" ]; then
        rm -f "$COORD_DIR/outage_start" "$COORD_DIR/outage_end" "$COORD_DIR/outage_restored" "$COORD_DIR/outage_done_"* 2>/dev/null || true
        signal "outage_start"
    fi

    wait_signal "outage_start" 300

    apply_netem_profile "blackhole"
    echo "[node${NODE_INDEX}] outage hold for ${OUTAGE_SEC}s"
    sleep "$OUTAGE_SEC"

    tc qdisc del dev eth0 root 2>/dev/null || true
    echo "[node${NODE_INDEX}] outage restored (netem removed)"
    signal "outage_done_${NODE_INDEX}"

    if [ "$NODE_INDEX" = "0" ]; then
        wait_all_signals "outage_done" 300
        signal "outage_restored"
    fi

    wait_signal "outage_restored" 300
}

run_mesh_phase() {
    local phase="$1"
    local duration_sec="$2"
    local max_reconnect_sec="$3"
    local netem_profile="$4"

    if [ "$NODE_INDEX" = "0" ]; then
        rm -f "$COORD_DIR/${phase}_start" "$COORD_DIR/${phase}_complete" "$COORD_DIR/${phase}_done_"* 2>/dev/null || true
        signal "${phase}_start"
    fi

    wait_signal "${phase}_start" 300

    apply_netem_profile "$netem_profile"
    local peers_csv
    peers_csv=$(get_peers_csv)

    if [ -z "$peers_csv" ]; then
        echo "[node${NODE_INDEX}] no peers found for phase $phase"
        touch "$COORD_DIR/FAIL"
        exit 1
    fi

    echo "[node${NODE_INDEX}] phase=$phase peers=$peers_csv"
    "$BIN_DIR/examples/mesh_check" \
        "0.0.0.0:31000" \
        "$MY_IP" \
        "$peers_csv" \
        "$duration_sec" \
        "$max_reconnect_sec" \
        "$MESH_TICK_MS" 2>&1 | tee -a "$LOG_DIR/mesh_${phase}.log"

    signal "${phase}_done_${NODE_INDEX}"

    if [ "$NODE_INDEX" = "0" ]; then
        wait_all_signals "${phase}_done" 600
        signal "${phase}_complete"
    fi

    wait_signal "${phase}_complete" 600
}

run_mesh_phase_nonfatal() {
    local phase="$1"
    local duration_sec="$2"
    local max_reconnect_sec="$3"
    local netem_profile="$4"

    set +e
    run_mesh_phase "$phase" "$duration_sec" "$max_reconnect_sec" "$netem_profile"
    local status=$?
    set -e

    if [ "$status" -ne 0 ]; then
        echo "[node${NODE_INDEX}] nonfatal mesh phase failed: $phase (exit=$status)"
    fi

    return 0
}

rolling_restart() {
    if [ "$NODE_INDEX" = "0" ]; then
        rm -f "$COORD_DIR/rolling_go" "$COORD_DIR/rolling_all_stopped" "$COORD_DIR/rolling_done" "$COORD_DIR/stopped_"* "$COORD_DIR/restarted_"* 2>/dev/null || true
        signal "rolling_go"
    fi

    wait_signal "rolling_go" 120

    local slot=$(( NODE_INDEX * ROLLING_SLOT_SEC ))
    sleep "$slot"

    stop_iroh
    signal "stopped_${NODE_INDEX}"

    if [ "$NODE_INDEX" = "0" ]; then
        wait_all_signals "stopped" 600
        signal "rolling_all_stopped"
    fi

    wait_signal "rolling_all_stopped" 600

    start_iroh "$LOG_DIR/iroh_after_rolling.log"
    wait_assigned_ip "$LOG_DIR/iroh_after_rolling.log" 180
    wait_tun_for_ip 120
    echo "$MY_IP" > "$COORD_DIR/node${NODE_INDEX}.ip"

    signal "restarted_${NODE_INDEX}"

    if [ "$NODE_INDEX" = "0" ]; then
        wait_all_signals "restarted" 600
        rebuild_peers_list
        signal "rolling_done"
    fi

    wait_signal "rolling_done" 600
}

simultaneous_restart() {
    if [ "$NODE_INDEX" = "0" ]; then
        rm -f "$COORD_DIR/sim_go" "$COORD_DIR/sim_all_stopped" "$COORD_DIR/sim_done" "$COORD_DIR/sim_stopped_"* "$COORD_DIR/sim_restarted_"* 2>/dev/null || true
        signal "sim_go"
    fi

    wait_signal "sim_go" 120

    local jitter=$(( RANDOM % (SIM_JITTER_MAX_SEC + 1) ))
    sleep "$jitter"

    stop_iroh
    signal "sim_stopped_${NODE_INDEX}"

    if [ "$NODE_INDEX" = "0" ]; then
        wait_all_signals "sim_stopped" 600
        signal "sim_all_stopped"
    fi

    wait_signal "sim_all_stopped" 600

    start_iroh "$LOG_DIR/iroh_after_sim.log"
    wait_assigned_ip "$LOG_DIR/iroh_after_sim.log" 180
    wait_tun_for_ip 120
    echo "$MY_IP" > "$COORD_DIR/node${NODE_INDEX}.ip"

    signal "sim_restarted_${NODE_INDEX}"

    if [ "$NODE_INDEX" = "0" ]; then
        wait_all_signals "sim_restarted" 600
        rebuild_peers_list
        signal "sim_done"
    fi

    wait_signal "sim_done" 600
}

if [ "$NODE_INDEX" = "0" ]; then
    rm -f "$COORD_DIR"/* 2>/dev/null || true
fi
sleep 1

start_iroh "$LOG_DIR/iroh.log"
wait_assigned_ip "$LOG_DIR/iroh.log" 180
wait_tun_for_ip 120

echo "$MY_IP" > "$COORD_DIR/node${NODE_INDEX}.ip"
signal "ready_${NODE_INDEX}"

if [ "$NODE_INDEX" = "0" ]; then
    wait_all_signals "ready" 300
    rebuild_peers_list
    signal "all_ready"
fi

wait_signal "all_ready" 300

run_mesh_phase "baseline" "$BASELINE_SEC" "$RECONNECT_MAX_SEC" "clean"
run_mesh_phase "degraded" "$DEGRADED_SEC" "$RECONNECT_MAX_SEC" "degraded"

rolling_restart
run_mesh_phase "post_rolling" "$POST_ROLLING_SEC" "$RECONNECT_MAX_SEC" "degraded"

simultaneous_restart
run_mesh_phase "post_simultaneous" "$POST_SIM_SEC" "$RECONNECT_MAX_SEC" "degraded"

# Extra chaos step: force hard outage (no recovery possible), then restore and verify recovery
run_outage_cycle
run_mesh_phase_nonfatal "post_outage_recovery" "$POST_OUTAGE_RECOVERY_SEC" "$OUTAGE_RECOVERY_MAX_SEC" "clean"

run_mesh_phase "recovery" "$RECOVERY_SEC" "$RECONNECT_MAX_SEC" "clean"

if [ "$NODE_INDEX" = "0" ]; then
    signal "TEST_PASS"
fi

wait_signal "TEST_PASS" 120

stop_iroh

echo "[node${NODE_INDEX}] reliability test complete"
exit 0
