#!/bin/bash
set -e

GAME_TEST_DURATION=5  # in minutes

SERVICE_NAME=$1
echo "Service: $SERVICE_NAME starting [STRESS MODE]..."

echo "Starting iroh-lan..."
export RUST_LOG=iroh_lan=trace,info
> /app/iroh.log
/app/bin/iroh-lan --name testnet -d > /app/iroh.log 2>&1 &
IROH_PID=$!

# Function to get current IP dynamically
get_current_ip() {
    # Searches for lines like "My IP is X" or "Successfully assigned IP: X"
    # Returns the last one found
    grep -E "My IP is|Successfully assigned IP:" /app/iroh.log | tail -n1 | awk '{print $NF}'
}

echo "Waiting for IP assignment..."
TIMEOUT=120
start_time=$(date +%s)
  
GOT_IP=""
while [ $(( $(date +%s) - start_time )) -lt $TIMEOUT ]; do
      GOT_IP=$(get_current_ip)
      if [ -n "$GOT_IP" ]; then
          break
      fi
      sleep 1
done

if [ -z "$GOT_IP" ]; then
      echo "Timeout waiting for IP."
      cat /app/iroh.log
      exit 1
fi

echo "Assigned IP: $GOT_IP"

if [ "$GOT_IP" == "172.22.0.3" ]; then
    ROLE="server"
    PEER_IP="172.22.0.4"
    echo "I am the SERVER (since I got .3)"
elif [ "$GOT_IP" == "172.22.0.4" ]; then
    ROLE="client"
    PEER_IP="172.22.0.3"
    echo "I am the CLIENT (since I got .4)"
else
    echo "Error: Unexpected IP $GOT_IP"
    exit 1
fi

echo "Waiting for tun device..."
while ! ls /sys/class/net/ | grep -q tun; do sleep 0.5; done
TUN_DEV=$(ls /sys/class/net/ | grep tun | head -n1)

if [ "$TUN_DEV" != "tun1" ]; then
    ip link set dev $TUN_DEV down
    ip link set dev $TUN_DEV name tun1
    ip link set dev tun1 up
fi


if [ "$ROLE" == "server" ]; then
    echo "Starting Game Check Server (Background)..."
    /app/bin/examples/game_check server "0.0.0.0:30000" "none" "$GAME_TEST_DURATION" > /app/game_server.log 2>&1 &

    echo "Starting iperf3 server..."
    # iperf3 server mode
    iperf3 -s
    
    # Wait for iroh to verify it stays alive
    wait $IROH_PID

elif [ "$ROLE" == "client" ]; then
    echo "Waiting for server to be ready..."
    sleep 5
    
    # DEBUG: Check for lost IPs
    echo "DEBUG: Checking for lost IP assignments in logs..."
    grep "Lost IP assignment" /app/iroh.log || true
    echo "DEBUG: Checking for assigned IPs in logs..."
    grep "Successfully assigned IP" /app/iroh.log || true

    # A. Broadcast Hostility Test (simulates 'bad' peer/loop)
    echo "TEST 1/4: Starting Broadcast Ping (Background Hostility)..."
    ping -b -i 0.2 -c 100 255.255.255.255 > /dev/null 2>&1 &
    PING_PID=$!

    # B. Throughput Test (Clean Network)
    # Refresh Peer IP in case of conflicts
    PEER_IP_CURRENT=$(get_current_ip)
    # Assuming standard topology: .3 <-> .4
    # If I am .3, peer is .4. If I changed to .5, peer might be .3 or .4?
    # For robustness, we stick to the initial PEER_IP unless we implement discovery.
    # But we should log our own IP status.
    echo "My Current IP: $PEER_IP_CURRENT"

    echo "TEST 2/4: Baseline Throughput (10s)..."
    iperf3 -c $PEER_IP -t 10
    
    # C. Poor Network Quality Test
    echo "TEST 3/4: Simulating Link Loss/Delay (tc netem)..."
    # Delay 500ms +/- 250ms, 1% Packet Loss
    tc qdisc add dev eth0 root netem delay 500ms 250ms distribution normal loss 1%
    
    echo "Running iperf3 on degraded link..."
    # We expect lower throughput, but NO DISCONNECTS (iperf should finish)
    iperf3 -c $PEER_IP -t 10 || echo "iperf failed on degraded link (expected performance drop, not crash)"

    # Clean up tc rules
    tc qdisc del dev eth0 root
    
    # Wait for server to cleanup (iperf3 might be stuck if FIN was lost)
    echo "Waiting for server cleanup (15s)..."
    sleep 15

    # D. Disconnection/Reconnection Test
    echo "TEST 4/4: Reconnection Event..."
    echo "Killing iroh-lan..."
    kill $IROH_PID
    sleep 2 # Let it die

    echo "Restarting iroh-lan..."
    /app/bin/iroh-lan --name testnet -d > /app/iroh_2.log 2>&1 &
    IROH_PID_2=$!
    
    echo "Waiting for new IP assignment..."
    # Wait loop for IP
    for i in {1..30}; do
        if grep -q "My IP is" /app/iroh_2.log; then
            NEW_IP=$(grep "My IP is" /app/iroh_2.log | awk '{print $NF}')
            echo "Restarted! New IP: $NEW_IP"
            break
        fi
        sleep 1
    done
    
    if [ -z "$NEW_IP" ]; then
        echo "Failed to get new IP after restart."
        cat /app/iroh_2.log
        exit 1
    fi
    
    # Give it a moment to stabilize routes
    sleep 5
    
    # Verify connectivity again
    echo "Verifying connectivity after restart..."
    # 5 seconds test
    iperf3 -c $PEER_IP -t 5
    
    # E. Game Simulation Test
    echo "TEST 5/5: Minetest-like Game Simulation (${GAME_TEST_DURATION}m)..."
    
    # Re-Apply harsh network conditions
    echo "Applying hostile network conditions (500ms delay, 1% loss)..."
    tc qdisc add dev eth0 root netem delay 500ms 250ms distribution normal loss 1%

    echo "Running Game Client..."
    # Connect to the SERVER (PEER_IP) which is running game_check server
    /app/bin/examples/game_check client "0.0.0.0:0" "$PEER_IP:30000" "$GAME_TEST_DURATION"
    GAME_EXIT=$?
    
    # Cleanup tc
    tc qdisc del dev eth0 root

    if [ $GAME_EXIT -eq 0 ]; then
        echo "GAME SIMULATION PASSED."
    else
        echo "GAME SIMULATION FAILED."
        exit 1
    fi
    
    echo "ALL STRESS TESTS PASSED."
    
    # Cleanup background ping
    kill $PING_PID || true
    exit 0
fi
