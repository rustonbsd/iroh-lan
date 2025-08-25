use std::{io::Read, time::Duration};

use iroh::{NodeId, SecretKey};
use tokio::{signal::ctrl_c, time::sleep};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    let secret = SecretKey::generate(&mut rand::thread_rng());

    let (remote_writer, mut remote_reader) = tokio::sync::broadcast::channel(1024); 
    let t = iroh_lan::Tun::new((0,5),secret.public(),remote_writer)?;

    let router = iroh_lan::Router::builder()
        .entry_name("my-lan-party")
        .secret_key(secret)
        .build()
        .await?;
    
    loop {
        tokio::select! {
            Ok(tun_recv) = remote_reader.recv() => {
                let _ = router.package_route(tun_recv).await;
            }
        }
    }
}
