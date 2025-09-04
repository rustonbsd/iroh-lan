use std::time::Duration;

use iroh::SecretKey;
use tokio::{task::JoinHandle, time::sleep};

use crate::DirectMessage;

pub async fn run(name: &str, password: &str, creator_mode: bool) -> anyhow::Result<(crate::Router, JoinHandle<()>)> {
    let secret = SecretKey::generate(&mut rand::thread_rng());
    let router = if creator_mode {
        crate::Router::builder()
            .entry_name(name)
            .password(password)
            .creator_mode()
            .secret_key(secret.clone())
            .build()
            .await?
    } else {
        crate::Router::builder()
            .entry_name(name)
            .password(password)
            .secret_key(secret.clone())
            .build()
            .await?
    };

    while router.my_ip().await.is_none() {
        println!("Waiting to get an IP address...");
        sleep(Duration::from_secs(1)).await;
    }

    let my_ip = router.my_ip().await.ok_or_else(|| anyhow::anyhow!("failed to get my IP"))?;
    println!("My IP address is {}", my_ip);


    let (remote_writer, mut remote_reader) = tokio::sync::mpsc::channel(1024*16);
    let tun = crate::Tun::new(
        (my_ip.octets()[2], my_ip.octets()[3]),
        remote_writer,
    )?;

    Ok((router.clone(),tokio::spawn(async move {
               
        let mut direct_rx = router.subscribe_direct_connect();

        loop {
            tokio::select! {
                Some(tun_recv) = remote_reader.recv() => {
                    if let Ok(remote_node_id)  = router.ip_to_node_id(tun_recv.clone()).await {
                        if let Err(err) = router.direct.route_packet(remote_node_id, DirectMessage::IpPacket(tun_recv)).await {
                            println!("[ERROR] failed to route packet to {:?}", remote_node_id);
                            println!("Reason: {:?}", err);
                        }
                    }
                }
                Ok(direct_msg) = direct_rx.recv() => {
                    match direct_msg {
                        DirectMessage::IpPacket(ip_pkg) => {
                            let _ = tun.write(ip_pkg).await;
                        }
                    }
                }
            }
        }
    })))
}
