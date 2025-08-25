use std::{io::Read, time::Duration};

use iroh::{NodeId, SecretKey};
use tokio::{signal::ctrl_c, time::sleep};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(())
    }
    
    let secret = SecretKey::generate(&mut rand::thread_rng());

    let mut router = iroh_lan::Router::builder()
        .entry_name("my-lan-party")
        .secret_key(secret)
        .creator_mode()
        .build()
        .await?;


    tokio::spawn({
        let mut router = router.clone();
        async move {
            let mut rec = router.gossip_receiver.subscribe().await.unwrap();
            loop {
                println!("msg: {:?}", rec.recv().await);
            }
        }
    });

    loop {
        println!("{:?}",router.get_state().await);
        sleep(Duration::from_secs(5)).await;
    }

    ctrl_c().await.unwrap();
    Ok(())
}
