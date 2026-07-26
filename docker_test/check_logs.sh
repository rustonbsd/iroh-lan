#!/bin/bash
# docker_test/check_logs.sh

# Find the latest server log directory (handling root ownership if needed)
LOG_DIR=$(sudo ls -td docker_test/results_stress/server/*/ 2>/dev/null | head -1)

if [ -z "$LOG_DIR" ]; then
    echo "No log directory found in docker_test/results_stress/server/"
    exit 0
fi

LOG_FILE="$LOG_DIR/iroh.log"
echo "Analyzing logs in: $LOG_DIR"

if sudo [ ! -f "$LOG_FILE" ]; then
    echo "Log file not found: $LOG_FILE"
    exit 0
fi

echo "========================================================"
echo "    DATA/CONTROL PLANE MISMATCH ANALYSIS"
echo "========================================================"

BLOB_MISS_COUNT=$(sudo grep -c "Pending blob sync for assignment" "$LOG_FILE")
LIVENESS_COUNT=$(sudo grep -c "Data-Plane Liveness" "$LOG_FILE")
DISCONNECT_COUNT=$(sudo grep -c "Peer disconnected" "$LOG_FILE")

if [ "$BLOB_MISS_COUNT" -gt 0 ]; then
    echo -e "\033[0;33m[!] CAUSE DETECTED:\033[0m Router skipped $BLOB_MISS_COUNT updates due to missing blobs."
    echo "    (This confirms the Metadata-Data race condition exists)"
else
    echo -e "\033[0;32m[OK]\033[0m Router Sync: No missing blobs detected."
fi

if [ "$LIVENESS_COUNT" -gt 0 ]; then
    echo -e "\033[0;32m[SUCCESS]\033[0m PROTECTION ENGAGED: Data-Plane Liveness saved $LIVENESS_COUNT routes."
    echo "    (Connections were preserved despite Router/Doc temporary sync state)"
else
    echo "[INFO] Protection Idle: Data-Plane Liveness logic not required this run."
fi

if [ "$DISCONNECT_COUNT" -gt 0 ]; then
     echo -e "\033[0;31m[FAIL]\033[0m Connectivity Unstable: $DISCONNECT_COUNT Disconnects detected."
else
     echo -e "\033[0;32m[PASS]\033[0m Connectivity Stable: 0 Disconnects."
fi
echo "========================================================"
