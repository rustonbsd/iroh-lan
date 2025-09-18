use iroh_lan::network::Network;
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

    tokio::spawn(async move {
        loop {
            println!(
                "Network started with node ID {:?}",
                network.get_router_state().await
            );
            sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    loop {
        tokio::select! {

            _ = tokio::signal::ctrl_c() => {
                break Ok(())
            }
        }
    }
    //Ok(())
}
