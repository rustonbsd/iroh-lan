use std::time::Duration;

use actor_helper::{act, act_ok, Action, Actor, Handle};
use anyhow::Result;
use iroh::{Endpoint, SecretKey};
use iroh_blobs::store::mem::MemStore;
use iroh_docs::protocol::Docs;
use iroh_gossip::{net::Gossip, proto::HyparviewConfig};
use tracing::warn;

use crate::{ 
    local_networking::Ipv4Pkg, router::RouterIp, Direct, DirectMessage, Router, Tun
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
                router,
                direct,

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

    pub async fn get_router_handle(&self) -> Result<Router> {
        self.api
            .call(act_ok!(actor => async move { actor.router.clone() }))
            .await
    }

    pub async fn get_node_id(&self) -> Result<iroh::NodeId> {
        self.api
            .call(act!(actor => actor.router.get_node_id()))
            .await
    }

    pub async fn get_peers(&self) -> Result<Vec<(iroh::NodeId,Option<std::net::Ipv4Addr>)>> {
        self.api
            .call(act!(actor => actor.router.get_peers()))
            .await
    }

    pub async fn get_direct_handle(&self) -> Result<Direct> {
        self.api
            .call(act_ok!(actor => async move { actor.direct.clone() }))
            .await
    }

    pub async fn close(&self) -> Result<()> {
        warn!("Closing network TODO!");

        self.api.call(act_ok!(actor => async move {
            let _ = actor._router.shutdown().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            //actor._endpoint.close().await;
        })).await
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
                        if let Ok(tun) = Tun::new((ip.octets()[2],ip.octets()[3]), self._local_to_direct_tx.clone()).await {
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