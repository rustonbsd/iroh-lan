#!/usr/bin/env bash
# Builds/destroys the NAT lab. Everything lives inside namespaces
# prefixed "il-" (iroh-lab). Teardown removes all of it.
set -euo pipefail

P="il"                       # namespace prefix, avoids clashing with anything
NODES=(0 1 2 3 4)
# which router each node sits behind:
ROUTER_OF=(0 0 1 1 2)
ROUTERS=(0 1 2)

UPLINK_SUBNET="10.250.0.0/30"
UPLINK_HOST_IP="10.250.0.1"
UPLINK_ISP_IP="10.250.0.2"
HOST_VETH="il-uplink"
REAL_IFACE="${REAL_IFACE:-}"

ns_node()   { echo "${P}-node$1"; }
ns_rtr()    { echo "${P}-rtr$1"; }
NS_ISP="${P}-isp"

teardown() {
    remove_uplink
    for ns in $(ip netns list | awk '{print $1}' | grep "^${P}-" || true); do
        ip netns del "$ns" 2>/dev/null || true
    done
    echo "lab removed. verify with: ip netns list"
}

setup() {
    teardown

    # ---- ISP namespace with a bridge (the fake internet) ----
    ip netns add "$NS_ISP"
    ip -n "$NS_ISP" link set lo up
    ip netns exec "$NS_ISP" sysctl -q -w net.ipv4.ip_forward=1
    ip -n "$NS_ISP" link add br0 type bridge
    ip -n "$NS_ISP" addr add 203.0.113.1/24 dev br0
    ip -n "$NS_ISP" link set br0 up

    # ---- routers ----
    for r in "${ROUTERS[@]}"; do
        local_rtr=$(ns_rtr "$r")
        ip netns add "$local_rtr"
        ip -n "$local_rtr" link set lo up

        # WAN link: router <-> isp bridge
        ip link add "${P}-w${r}" type veth peer name wan netns "$local_rtr"
        ip link set "${P}-w${r}" netns "$NS_ISP"
        ip -n "$NS_ISP" link set "${P}-w${r}" master br0
        ip -n "$NS_ISP" link set "${P}-w${r}" up

        ip -n "$local_rtr" addr add "203.0.113.$(( (r + 1) * 10 ))/24" dev wan
        ip -n "$local_rtr" link set wan up
        ip -n "$local_rtr" route add default via 203.0.113.1

        # LAN side: a bridge inside the router ns
        ip -n "$local_rtr" link add lan type bridge
        ip -n "$local_rtr" addr add "192.168.$(( (r + 1) * 10 )).1/24" dev lan
        ip -n "$local_rtr" link set lan up

        ip netns exec "$local_rtr" sysctl -q -w net.ipv4.ip_forward=1

        # NAT: masquerade LAN -> WAN. Lives ONLY in this namespace.
        ip netns exec "$local_rtr" nft -f - <<EOF
table ip nat {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        oifname "wan" masquerade
    }
}
EOF
    done

    # ---- nodes ----
    for i in "${NODES[@]}"; do
        r=${ROUTER_OF[$i]}
        subnet=$(( (r + 1) * 10 ))
        node=$(ns_node "$i")
        rtr=$(ns_rtr "$r")

        ip netns add "$node"
        ip -n "$node" link set lo up

        ip link add "${P}-n${i}" type veth peer name eth0 netns "$node"
        ip link set "${P}-n${i}" netns "$rtr"
        ip -n "$rtr" link set "${P}-n${i}" master lan
        ip -n "$rtr" link set "${P}-n${i}" up

        ip -n "$node" addr add "192.168.${subnet}.10$((i + 1))/24" dev eth0
        ip -n "$node" link set eth0 up
        ip -n "$node" route add default via "192.168.${subnet}.1"
        mkdir -p "/etc/netns/${node}"
        echo "nameserver 1.1.1.1" > "/etc/netns/${node}/resolv.conf"
    done

    echo "lab up:"
    ip netns list | grep "^${P}-"
    if [ "${WITH_UPLINK:-1}" = "1" ]; then
        add_uplink
        echo "uplink added via $HOST_VETH (host $UPLINK_HOST_IP <-> isp $UPLINK_ISP_IP)"
    fi
}

status() {
    for ns in $(ip netns list | awk '{print $1}' | grep "^${P}-" || true); do
        echo "== $ns =="
        ip -n "$ns" -o -4 addr show | awk '{print "  " $2, $4}'
    done
}

detect_real_iface() {
    if [ -z "$REAL_IFACE" ]; then
        REAL_IFACE=$(ip route show default | awk '/default/ {print $5; exit}')
    fi
    echo "using host uplink interface: $REAL_IFACE"
}

add_uplink() {
    detect_real_iface

    # host-side leg of the veth stays on the host, ISP-side moves into the ns
    ip link add "$HOST_VETH" type veth peer name uplink netns "$NS_ISP"
    ip addr add "${UPLINK_HOST_IP}/30" dev "$HOST_VETH"
    ip link set "$HOST_VETH" up

    ip -n "$NS_ISP" addr add "${UPLINK_ISP_IP}/30" dev uplink
    ip -n "$NS_ISP" link set uplink up
    ip -n "$NS_ISP" route add default via "$UPLINK_HOST_IP"

    # ISP: masquerade all lab traffic leaving via the uplink so the host only sees 10.250.0.0/30
    ip netns exec "$NS_ISP" nft -f - <<EOF
table ip nat {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        oifname "uplink" masquerade
    }
}
EOF

    # host: allow forwarding, NAT only for traffic FROM this exact subnet
    sysctl -q -w net.ipv4.ip_forward=1
    iptables -t nat -A POSTROUTING -s "$UPLINK_SUBNET" -o "$REAL_IFACE" \
        -j MASQUERADE -m comment --comment "il-lab-uplink"
    iptables -A FORWARD -s "$UPLINK_SUBNET" -o "$REAL_IFACE" -j ACCEPT \
        -m comment --comment "il-lab-uplink"
    iptables -A FORWARD -d "$UPLINK_SUBNET" -i "$REAL_IFACE" -m state \
        --state ESTABLISHED,RELATED -j ACCEPT -m comment --comment "il-lab-uplink"

    # DNS: reuse host resolver via the same host IP, simplest correct option
    mkdir -p /tmp/il-resolv
    echo "nameserver ${UPLINK_HOST_IP}" > /tmp/il-resolv/resolv.conf
    # actually forward DNS: host must accept/forward UDP/TCP 53 too, covered
    # by the generic FORWARD/MASQUERADE rules above since port isn't filtered.
    cp /tmp/il-resolv/resolv.conf /etc/netns/${NS_ISP}/resolv.conf 2>/dev/null || {
        mkdir -p "/etc/netns/${NS_ISP}"
        echo "nameserver ${UPLINK_HOST_IP}" > "/etc/netns/${NS_ISP}/resolv.conf"
    }
}

remove_uplink() {
    ip netns exec "$NS_ISP" nft delete table ip nat 2>/dev/null || true
    ip link del "$HOST_VETH" 2>/dev/null || true
    iptables -t nat -S POSTROUTING 2>/dev/null | grep "il-lab-uplink" | while read -r rule; do
        eval "iptables -t nat -D POSTROUTING ${rule#-A POSTROUTING }" 2>/dev/null || true
    done || true
    iptables -S FORWARD 2>/dev/null | grep "il-lab-uplink" | while read -r rule; do
        eval "iptables -D FORWARD ${rule#-A FORWARD }" 2>/dev/null || true
    done || true
    rm -rf "/etc/netns/${NS_ISP}"
}

case "${1:-}" in
    setup)    setup ;;
    teardown) teardown ;;
    status)   status ;;
    *) echo "usage: $0 setup|teardown|status"; exit 1 ;;
esac