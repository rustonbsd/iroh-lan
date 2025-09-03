use iroh::SecretKey;
use iroh_lan::DirectMessage;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::sleep,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    let secret = SecretKey::generate(&mut rand::thread_rng());
    let router = iroh_lan::Router::builder()
        .entry_name("my-lan-party")
        .creator_mode()
        .secret_key(secret.clone())
        .build()
        .await?;

    while router.node_id_to_ip(router.node_id()).await.is_err() {
        sleep(Duration::from_secs(1)).await;
    }

    let my_ip = router.node_id_to_ip(router.node_id()).await?;

    let (remote_writer, mut remote_reader) = tokio::sync::mpsc::channel(1024*16);
    let tun = iroh_lan::Tun::new(
        (my_ip.octets()[2], my_ip.octets()[3]),
        remote_writer,
    )?;
    let mut direct_rx = router.subscribe_direct_connect();

    println!("My IP: {}", my_ip);

    loop {
        tokio::select! {
            Some(tun_recv) = remote_reader.recv() => {
                if let Ok(remote_node_id)  = router.ip_to_node_id(tun_recv.clone()).await {
                    if let Err(err) = router.direct.route_packet(remote_node_id, DirectMessage::IpPacket(tun_recv)).await {
                        println!("[ERROR] failed to route packet to {:?}", remote_node_id);
                        println!("Reason: {:?}", err);
                    } else {
                        //println!("Routed packet to {:?}", remote_node_id);
                    }
                }
            }
            Ok(direct_msg) = direct_rx.recv() => {
                match direct_msg {
                    DirectMessage::IpPacket(ip_pkg) => {
                        //println!("WRITE TUN: {:?}", ip_pkg.to_ipv4_packet()?.get_destination());
                        //println!("remote reader subs: {}",remote_reader.capacity());
                        tun.write(ip_pkg).await?;
                    }
                }
            }
        }
    }
}
