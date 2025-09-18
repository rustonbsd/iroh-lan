# iroh-lan

Have a lan party with iroh + tun cross platform

Very work in progress!

Status
- Transport: overhauled and fully working
- Tauri UI: early work-in-progress, coming soon

This PR merges the refactored transport code so it can be used from main even though the UI is not yet complete.

## Goals
- *pure* p2p hamachi with no servers and no accounts.
- just enter a network name and password and anyone who enters the same will network their L3 (specifically UDP, TCP and ICMP) packages with you as if you are in the same subnet.

## some network infos

    network: 172.22.0.0/24
    usable host range: 172.22.0.1 - 172.22.0.254
    subnet mask: 255.255.255.0
