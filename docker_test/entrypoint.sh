#!/bin/bash
set -e

SERVICE_NAME=$1
echo "Service: $SERVICE_NAME starting..."

echo "Starting iroh-lan..."
export RUST_LOG=debug
> /app/iroh.log
/app/bin/iroh-lan --name testnet -d > /app/iroh.log 2>&1 &
IROH_PID=$!

echo "Waiting for IP assignment..."
TIMEOUT=120
start_time=$(date +%s)
  
GOT_IP=""
while [ $(( $(date +%s) - start_time )) -lt $TIMEOUT ]; do
      if grep -q "My IP is" /app/iroh.log; then
          GOT_IP=$(grep "My IP is" /app/iroh.log | tail -n1 | awk '{print $NF}')
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

# Determine ROLE based on IP
if [ "$GOT_IP" == "172.22.0.3" ]; then
    ROLE="server"
    echo "I am the SERVER (since I got .3)"
elif [ "$GOT_IP" == "172.22.0.4" ]; then
    ROLE="client"
    echo "I am the CLIENT (since I got .4)"
else
    echo "Error: Unexpected IP $GOT_IP. Expected 172.22.0.3 or 172.22.0.4."
    cat /app/iroh.log
    exit 1
fi

# Find tun device
# Wait for tun device to appear (it should be there after iroh starts)
echo "Waiting for tun device..."
while ! ls /sys/class/net/ | grep -q tun; do
    sleep 0.5
done
TUN_DEV=$(ls /sys/class/net/ | grep tun | head -n1)
echo "Found tun device: $TUN_DEV"

# Examples hardcode 'tun1', so we must ensure the interface is named tun1
if [ "$TUN_DEV" != "tun1" ]; then
    echo "Renaming $TUN_DEV to tun1..."
    ip link set dev $TUN_DEV down
    ip link set dev $TUN_DEV name tun1
    ip link set dev tun1 up
fi

if [ "$ROLE" == "server" ]; then
    echo "Starting TCP Server example..."
    /app/bin/examples/tcp_server &
    SERVER_PID=$!
    wait $SERVER_PID
elif [ "$ROLE" == "client" ]; then
    echo "Waiting for server (assuming it's up)..."
    sleep 5
    echo "Starting TCP Client example (running for 10s to verify connection)..."
    # Run for 10 seconds. If it's still running (exit 124), that means connection is likely stable (looping).
    # If it fails to connect, it crashes with exit 1 immediately.
    timeout 10s /app/bin/examples/tcp_client || EXIT_CODE=$?
    
    if [ $EXIT_CODE -eq 124 ]; then
        echo "SUCCESS: Client maintained connection for 10s."
        exit 0
    elif [ $EXIT_CODE -eq 0 ]; then
         echo "Client exited successfully."
         exit 0
    else
        echo "FAILURE: Client exited with error code $EXIT_CODE"
        exit 1
    fi
fi
