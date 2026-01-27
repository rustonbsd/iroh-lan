use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use actor_helper::{Action, Actor, Handle, act, act_ok};
use anyhow::Result;
use iroh::{Endpoint, EndpointId, SecretKey};
use iroh_blobs::store::mem::MemStore;
use iroh_docs::protocol::Docs;
use iroh_gossip::{net::Gossip, proto::HyparviewConfig};
use sha2::Digest;
use tracing::{debug, error, info, trace, warn};

use crate::{Direct, DirectMessage, Router, Tun, local_networking::Ipv4Pkg, router::RouterIp};

const PENDING_TTL: Duration = Duration::from_secs(60);
const PENDING_MAX_PER_IP: usize = 1024*16;

#[derive(Debug, Clone)]
pub struct Network {
    api: Handle<NetworkActor, anyhow::Error>,
}

#[derive(Debug)]
struct NetworkActor {
    rx: actor_helper::Receiver<Action<NetworkActor>>,

    router: Router,
    direct: Direct,

    _iroh_router: iroh::protocol::Router,
    _iroh_endpoint: iroh::endpoint::Endpoint,

    tun: Option<Tun>,

    _local_to_direct_tx: tokio::sync::mpsc::Sender<Ipv4Pkg>,
    local_to_direct_rx: tokio::sync::mpsc::Receiver<Ipv4Pkg>,

    _direct_to_local_tx: tokio::sync::mpsc::Sender<DirectMessage>,
    direct_to_local_rx: tokio::sync::mpsc::Receiver<DirectMessage>,

    ip_cache: HashMap<std::net::Ipv4Addr, EndpointId>,
    peer_ids: HashSet<EndpointId>,
    pending_packets: HashMap<std::net::Ipv4Addr, VecDeque<(Instant, Ipv4Pkg)>>,
}

impl Network {
    pub async fn new(name: &str, password: &str) -> Result<Self> {
        let secret_key = SecretKey::generate(&mut rand::rng());

        let endpoint = Endpoint::builder()
            .secret_key(secret_key.clone())
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

        let mut network_secret = sha2::Sha512::new();
        network_secret.update(format!("iroh-lan-network-name-{}", name));
        network_secret.update(format!("iroh-lan-network-secret-{password}"));
        let network_secret: [u8; 64] = network_secret.finalize().into();

        let (direct_connect_tx, direct_connect_rx) = tokio::sync::mpsc::channel(1024 * 16);
        let direct = Direct::new(endpoint.clone(), direct_connect_tx.clone(), &network_secret);

        let _router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(crate::Direct::ALPN, direct.clone())
            .accept(iroh_docs::ALPN, docs.clone())
            .accept(
                iroh_blobs::ALPN,
                iroh_blobs::BlobsProtocol::new(&blobs, None),
            )
            .spawn();

        let router = crate::Router::builder()
            .entry_name(name)
            .password(password)
            .secret_key(secret_key)
            .endpoint(endpoint.clone())
            .gossip(gossip)
            .docs(docs)
            .blobs(blobs)
            .build()
            .await?;

        direct.set_router(router.clone()).await?;

        let (api, rx) = Handle::channel();
        tokio::spawn(async move {
            let (to_remote_writer, to_remote_reader) = tokio::sync::mpsc::channel(1024 * 16);
            let mut actor = NetworkActor {
                rx,
                router,
                direct,

                _iroh_router: _router,
                _iroh_endpoint: endpoint,

                tun: None,
                _local_to_direct_tx: to_remote_writer,
                local_to_direct_rx: to_remote_reader,
                _direct_to_local_tx: direct_connect_tx,
                direct_to_local_rx: direct_connect_rx,

                ip_cache: HashMap::new(),
                peer_ids: HashSet::new(),
                pending_packets: HashMap::new(),
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

    pub async fn get_node_id(&self) -> Result<EndpointId> {
        self.api
            .call(act!(actor => actor.router.get_node_id()))
            .await
    }

    pub async fn get_peers(&self) -> Result<Vec<(EndpointId, Option<std::net::Ipv4Addr>)>> {
        self.api.call(act!(actor => actor.router.get_peers())).await
    }

    pub async fn get_direct_handle(&self) -> Result<Direct> {
        self.api
            .call(act_ok!(actor => async move { actor.direct.clone() }))
            .await
    }

    pub async fn close(&self) -> Result<()> {
        warn!("Closing network TODO!");

        self.api
            .call(act_ok!(actor => async move {
                let _ = actor._iroh_router.shutdown().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                //actor._endpoint.close().await;
            }))
            .await
    }
}

impl Actor<anyhow::Error> for NetworkActor {
    async fn run(&mut self) -> Result<()> {
        let mut ip_tick = tokio::time::interval(Duration::from_millis(500));
        ip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut cache_tick = tokio::time::interval(Duration::from_secs(1));
        cache_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("NetworkActor started");

        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                   trace!("NetworkActor action received");
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl-C, shutting down NetworkActor");
                    break
                }

                // init tun after ip is assigned
                _ = ip_tick.tick() => {
                    if self.tun.is_none() {
                        match self.router.get_ip_state().await {
                            Ok(RouterIp::AssignedIp(ip)) => {
                                info!("Initializing TUN for IP: {}", ip);
                                match tokio::time::timeout(
                                    Duration::from_secs(10),
                                    Tun::new((ip.octets()[2],ip.octets()[3]), self._local_to_direct_tx.clone())
                                ).await {
                                    Ok(Ok(tun)) => {
                                        info!("TUN initialized successfully");
                                        self.tun = Some(tun);
                                        if let Ok(peers) = self.router.get_peers().await {
                                            for (id, _) in peers {
                                                info!("Ensuring direct connection after IP assignment: {}", id);
                                                let direct = self.direct.clone();
                                                tokio::spawn(async move {
                                                    if let Err(e) = direct.ensure_connection(id).await {
                                                        warn!("Failed to ensure connection to {}: {}", id, e);
                                                    }
                                                });
                                            }
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        error!("Failed to initialize TUN: {}", e);
                                    }
                                    Err(_) => {
                                        error!("Timed out initializing TUN");
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!("Failed to get router state while waiting for TUN: {}", e);
                            }
                        }
                    }
                }

                _ = cache_tick.tick() => {
                    if let Ok(peers) = self.router.get_peers().await {
                        let mut next_peer_ids = HashSet::new();
                        self.ip_cache.clear();
                        for (id, maybe_ip) in peers {
                            next_peer_ids.insert(id);
                            if let Some(ip) = maybe_ip {
                                self.ip_cache.insert(ip, id);
                            }
                        }
                        if !self.pending_packets.is_empty() {
                            let now = Instant::now();
                            let pending_keys: Vec<_> = self.pending_packets.keys().copied().collect();
                            for ip in pending_keys {
                                if let Some(queue) = self.pending_packets.get_mut(&ip) {
                                    queue.retain(|(ts, _)| now.duration_since(*ts) <= PENDING_TTL);
                                    if queue.is_empty() {
                                        self.pending_packets.remove(&ip);
                                        continue;
                                    }
                                }
                                if let Some((id, mut queue)) = self
                                    .ip_cache
                                    .get(&ip)
                                    .copied()
                                    .zip(self.pending_packets.remove(&ip))
                                {
                                    while let Some((_, pkt)) = queue.pop_front() {
                                        let direct = self.direct.clone();
                                        tokio::spawn(async move {
                                            if let Err(e) = direct.route_packet(id, DirectMessage::IpPacket(pkt)).await {
                                                warn!("Failed to route buffered packet to {}: {}", id, e);
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        for id in next_peer_ids.difference(&self.peer_ids).copied() {
                            info!("New peer discovered: {}. Ensuring direct connection", id);
                            let direct = self.direct.clone();
                            tokio::spawn(async move {
                                if let Err(e) = direct.ensure_connection(id).await {
                                    warn!("Failed to ensure connection to {}: {}", id, e);
                                }
                            });
                        }
                        self.peer_ids = next_peer_ids;
                    }
                }

                res = self.local_to_direct_rx.recv(), if self.tun.is_some() => {
                    match res {
                        Some(tun_recv) => {
                            // Tun initialized, route packets
                            if let Ok(ip_pkg) = tun_recv.to_ipv4_packet() {
                                let dest = ip_pkg.get_destination();
                                let to = if let Some(id) = self.ip_cache.get(&dest) {
                                    Ok(*id)
                                } else {
                                    self.router.get_endpoint_id_from_ip(dest).await
                                };
                                match to {
                                    Ok(to) => {
                                        trace!("Routing packet from TUN to {}", to);
                                        if let Err(e) = self.direct.route_packet(to, DirectMessage::IpPacket(tun_recv)).await {
                                            warn!("Failed to route packet to {}: {}", to, e);
                                        }
                                    }
                                    Err(e) => {
                                        trace!("Could not resolve endpoint for IP {}: {}", dest, e);
                                        let queue = self.pending_packets.entry(dest).or_default();
                                        if queue.len() >= PENDING_MAX_PER_IP {
                                            queue.pop_front();
                                        }
                                        queue.push_back((Instant::now(), tun_recv));
                                    }
                                }
                            }
                        }
                        None => {
                              error!("TUN channel closed, breaking loop");
                              break;
                        }
                    }
                }

                res = self.direct_to_local_rx.recv(), if self.tun.is_some() => {
                    match res {
                        Some(direct_msg) => {
                            // Route remote packet to tun if our ip
                            if let Some(tun) = &self.tun
                                && let DirectMessage::IpPacket(ip_pkg) = direct_msg {
                                    trace!("Routing remote packet to TUN");
                                    if let Err(e) = tun.write(ip_pkg).await {
                                        warn!("Failed to write to TUN: {}", e);
                                    }
                                }
                        }
                        None => {
                            warn!("NetworkActor direct channel closed");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
