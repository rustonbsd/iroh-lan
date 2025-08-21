use std::{io::Read, thread::sleep, time::Duration};

use iroh::{NodeId, SecretKey};
use tokio::signal::ctrl_c;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(())
    }
    let secret = SecretKey::generate(&mut rand::thread_rng());

    //let (remote_writer,remote_reader) = tokio::sync::broadcast::channel(1024); 

    //let t = iroh_lan::Tun::new((0,5),secret.public(),remote_writer)?;

    let router = iroh_lan::Router::builder().build().await?;

    ctrl_c().await.unwrap();
    Ok(())
}
