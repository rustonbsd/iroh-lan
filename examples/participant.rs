use std::{io::Read, time::Duration};

use iroh::{NodeId, SecretKey};
use iroh_lan::DirectMessage;
use tokio::{signal::ctrl_c, time::sleep};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    let secret = SecretKey::generate(&mut rand::thread_rng());

    let router = iroh_lan::Router::builder()
        .entry_name("my-lan-party")
        .secret_key(secret.clone())
        .build()
        .await?;

    while router.node_id_to_ip(router.node_id()).await.is_err() {
        sleep(Duration::from_secs(1)).await;
    }

    let my_ip = router.node_id_to_ip(router.node_id()).await?;

    let (remote_writer, mut remote_reader) = tokio::sync::broadcast::channel(1024);
    let tun = iroh_lan::Tun::new((my_ip.octets()[2], my_ip.octets()[3]), secret.public(), remote_writer)?;
    let mut direct_rx = router.subscribe_direct_connect();
    loop {
        tokio::select! {
            Ok(tun_recv) = remote_reader.recv() => {
                if let Ok(remote_node_id)  = router.ip_to_node_id(tun_recv.clone()).await {
                    if router.direct.route_packet(remote_node_id, DirectMessage::IpPacket(tun_recv)).await.is_err() {
                        println!("[ERROR] failed to route packet to {:?}", remote_node_id);
                    }
                }
            }
            Ok(direct_msg) = direct_rx.recv() => {
                match direct_msg {
                    DirectMessage::IpPacket(ip_pkg) => {
                        tun.write(ip_pkg).await?;
                    }
                }
            }
        }
    }
}
