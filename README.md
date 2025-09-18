# iroh-lan

Have a lan party with iroh + tun cross platform

Very work in progress!

Status
- Transport: overhauled and fully working
- Tauri UI: early work-in-progress, coming soon

This PR merges the refactored transport code so it can be used from main even though the UI is not yet complete.


## Example

```rust
use iroh_lan::{RouterIp, network::Network};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // elevate cross platform
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }
    
    // Create or join a self-bootstrapping L3 network.
    // Waits until at least one other peer is found.
    // All of this happens fully automatically.
    // This can take a couple of seconds for new networks because
    // peers need to propagate the mainline records across
    // the world first (see distributed-topic-tracker for details).
    let network = Network::new("network-name", "<password>").await?;

    while matches!(
        network.get_router_state().await?,
        RouterIp::NoIp | RouterIp::AquiringIp(_, _)
    ) {
        sleep(std::time::Duration::from_millis(500)).await;
    }

    println!("my ip is {:?}", network.get_router_state().await?);

    // as long as the network handle is kept alive you have a fully functioning L3 virtual lan proxy like hamachi
}
```


## Goals
- *pure* p2p hamachi with no servers and no accounts.
- just enter a network name and password and anyone who enters the same will network their L3 (specifically UDP, TCP and ICMP) packages with you as if you are in the same subnet.

## some network infos

    network: 172.22.0.0/24
    usable host range: 172.22.0.1 - 172.22.0.254
    subnet mask: 255.255.255.0
