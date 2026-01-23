use iroh_lan::{RouterIp, network::Network};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_thread_ids(true)
        .init();

    let network = Network::new("test1", "<password>").await?;

    while matches!(
        network.get_router_state().await?,
        RouterIp::NoIp | RouterIp::AquiringIp(_, _)
    ) {
        sleep(std::time::Duration::from_millis(500)).await;
    }

    if let RouterIp::AssignedIp(ip) = network.get_router_state().await? {
        println!("My IP is {}", ip);
    }

    tokio::spawn(async move {
        loop {
            if let Ok(RouterIp::AssignedIp(ip)) = network.get_router_state().await {
                 println!("My IP is still {}", ip);
            }
            sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    let _ = tokio::signal::ctrl_c().await;
    Ok(())
}
