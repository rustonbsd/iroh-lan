#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"

overall_fail=0
phases=(baseline degraded post_rolling post_simultaneous post_outage_recovery recovery)
optional_phases=(post_outage_recovery)

is_optional_phase() {
    local phase="$1"
    for optional in "${optional_phases[@]}"; do
        if [ "$optional" = "$phase" ]; then
            return 0
        fi
    done
    return 1
}

for node in 0 1 2 3 4; do
    base="./results/node${node}"
    latest=$(find "$base" -mindepth 1 -maxdepth 1 -type d -name '20[0-9][0-9][0-1][0-9][0-3][0-9]_[0-2][0-9][0-5][0-9][0-5][0-9]' -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n1 | cut -d' ' -f2- || true)

    if [ -z "$latest" ]; then
        echo "[FAIL] node${node}: no timestamped log directory found"
        overall_fail=1
        continue
    fi

    echo "node${node}: $latest"

    for phase in "${phases[@]}"; do
        log="$latest/mesh_${phase}.log"
        if [ ! -f "$log" ]; then
            if is_optional_phase "$phase"; then
                echo "  [INFO] ${phase}: missing log"
            else
                echo "  [FAIL] ${phase}: missing log"
                overall_fail=1
            fi
            continue
        fi

        if grep -q "MESH_CHECK: PASS" "$log"; then
            echo "  [PASS] ${phase}"
        else
            if is_optional_phase "$phase"; then
                echo "  [INFO] ${phase}"
            else
                echo "  [FAIL] ${phase}"
                overall_fail=1
            fi
            tail -n 20 "$log" || true
        fi
    done

done

echo "========================================================"
if [ "$overall_fail" -eq 0 ]; then
    echo "RELIABILITY RESULT: PASS"
    exit 0
else
    echo "RELIABILITY RESULT: FAIL"
    exit 1
fi
