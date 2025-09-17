use std::time::Duration;

use anyhow::Result;
use iroh::{Endpoint, SecretKey};
use iroh_blobs::store::mem::MemStore;
use iroh_docs::protocol::Docs;
use iroh_gossip::{net::Gossip, proto::HyparviewConfig};

use crate::{
    act, actor::{Action, Actor, Handle}, local_networking::Ipv4Pkg, router::RouterIp, Direct, DirectMessage, Router, Tun
};

#[derive(Debug, Clone)]
pub struct Network {
    api: Handle<NetworkActor>,
}

#[derive(Debug)]
struct NetworkActor {
    rx: tokio::sync::mpsc::Receiver<Action<NetworkActor>>,

    router: Router,
    direct: Direct,

    _router: iroh::protocol::Router,
    _endpoint: iroh::endpoint::Endpoint,

    tun: Option<Tun>,

    _local_to_direct_tx: tokio::sync::broadcast::Sender<Ipv4Pkg>,
    local_to_direct_rx: tokio::sync::broadcast::Receiver<Ipv4Pkg>,

    _direct_to_local_tx: tokio::sync::broadcast::Sender<DirectMessage>,
    direct_to_local_rx: tokio::sync::broadcast::Receiver<DirectMessage>,
}

impl Network {
    pub async fn new(name: &str, password: &str) -> Result<Self> {
        let secret = SecretKey::generate(&mut rand::thread_rng());

        let endpoint = Endpoint::builder()
            .discovery_n0()
            .secret_key(secret.clone())
            .bind()
            .await?;

        let gossip_config = HyparviewConfig {
            neighbor_request_timeout: Duration::from_millis(2000),
            ..Default::default()
        };

        let gossip = Gossip::builder()
            .membership_config(gossip_config)
            .spawn(endpoint.clone());

        let blobs = MemStore::default();
        let docs = Docs::memory()
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;

        let (direct_connect_tx, direct_connect_rx) = tokio::sync::broadcast::channel(1024 * 16);
        let direct = Direct::new(endpoint.clone(), direct_connect_tx.clone());

        let _router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(crate::Direct::ALPN, direct.clone())
            .accept(iroh_docs::ALPN, docs.clone())
            .accept(
                iroh_blobs::ALPN,
                iroh_blobs::BlobsProtocol::new(&blobs, endpoint.clone(), None),
            )
            .spawn();

        let router = crate::Router::builder()
            .entry_name(name)
            .password(password)
            .secret_key(secret.clone())
            .endpoint(endpoint.clone())
            .gossip(gossip)
            .docs(docs)
            .blobs(blobs)
            .build()
            .await?;

        let (api, rx) = Handle::<NetworkActor>::channel(1024 * 16);
        tokio::spawn(async move {
            let (to_remote_writer, to_remote_reader) = tokio::sync::broadcast::channel(1024 * 16);
            let mut actor = NetworkActor {
                rx,
                router: router,
                direct: direct,

                _router,
                _endpoint: endpoint,

                tun: None,
                _local_to_direct_tx: to_remote_writer,
                local_to_direct_rx: to_remote_reader,
                _direct_to_local_tx: direct_connect_tx,
                direct_to_local_rx: direct_connect_rx,
            };
            let _ = actor.run().await;
        });

        Ok(Self { api })
    }

    pub async fn get_router_state(&self) -> Result<RouterIp> {
        self.api
            .call(act!(actor => actor.router.get_ip_state()))
            .await
    }
}

impl Actor for NetworkActor {
    async fn run(&mut self) -> Result<()> {
        let mut ip_tick = tokio::time::interval(Duration::from_millis(500));
        ip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(action) = self.rx.recv() => {
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    break
                }

                // init tun after ip is assigned
                _ = ip_tick.tick(), if self.tun.is_none() && matches!(self.router.get_ip_state().await, Ok(RouterIp::AssignedIp(_))) => {
                    if let Ok(RouterIp::AssignedIp(ip)) = self.router.get_ip_state().await {
                        if let Ok(tun) = Tun::new((ip.octets()[2],ip.octets()[3]), self._local_to_direct_tx.clone()) {
                            self.tun = Some(tun);
                        }
                    }
                }

                Ok(tun_recv) = self.local_to_direct_rx.recv(), if self.tun.is_some() => {
                    // Tun initialized, route packets
                    if let Ok(ip_pkg) = tun_recv.to_ipv4_packet() {
                        let dest = ip_pkg.get_destination();
                        if let Ok(to) = self.router.get_node_id_from_ip(dest).await {
                            let _ = self.direct.route_packet(to, DirectMessage::IpPacket(tun_recv)).await;
                        }
                    }
                }

                Ok(direct_msg) = self.direct_to_local_rx.recv(), if self.tun.is_some() => {
                    // Route remote packet to tun if our ip
                    if let Some(tun) = &self.tun {
                        if let DirectMessage::IpPacket(ip_pkg) = direct_msg {
                            let _ = tun.write(ip_pkg).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

/*
pub async fn run(
    name: &str,
    password: &str,
    creator_mode: bool,
) -> anyhow::Result<(crate::Router, JoinHandle<()>)> {
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

    Ok((
        router.clone(),
        tokio::spawn(async move {
            let mut direct_rx = router.subscribe_direct_connect();
            let tun = router.tun.clone().expect("tun device not set").clone();
            let mut remote_reader = tun.subscribe().await.expect("subscribe to work");
            println!("Started standalone router event loop");
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
        }),
    ))
}
 */
