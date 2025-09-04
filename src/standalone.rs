use iroh::SecretKey;
use tokio::task::JoinHandle;

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

    Ok((router.clone(),tokio::spawn(async move {
               
        let mut direct_rx = router.subscribe_direct_connect();
        let tun = router.tun.clone().expect("tun device not set").clone();
        let mut remote_reader = tun.subscribe().await.expect("subscribe to work");
        loop {
            tokio::select! {
                Ok(tun_recv) = remote_reader.recv() => {
                    if let Ok(remote_node_id)  = router.ip_to_node_id(tun_recv.clone()).await {
                        if let Err(err) = router.direct.route_packet(remote_node_id, DirectMessage::IpPacket(tun_recv)).await {
                            tokio::spawn({
                                let router = router.clone();
                                async move {
                                    println!("Creating new connection to peer {}", remote_node_id);
                                    if let Ok(quic_conn) = router.direct.get_endpoint().await.connect(remote_node_id, crate::Direct::ALPN).await {
                                        let _ = router.direct.handle_connection(quic_conn).await;
                                    }
                            }});
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
