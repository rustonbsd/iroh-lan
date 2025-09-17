use iroh::SecretKey;
use iroh_lan::network::Network;

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

    let secret = SecretKey::generate(&mut rand::thread_rng());
    let network = Network::new("test1", "<password>").await?;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                break Ok(())
            }
        }
    
    }
    //Ok(())
}
