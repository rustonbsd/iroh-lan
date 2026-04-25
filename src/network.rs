use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

use actor_helper::{Action, Handle, Receiver, act, act_ok};
use anyhow::Result;
use iroh::{
    Endpoint, EndpointId, SecretKey, endpoint::{Connection, IdleTimeout, QuicTransportConfig, VarInt}, protocol::ProtocolHandler
};
use iroh_auth::Authenticator;

use iroh_gossip::{net::Gossip, proto::HyparviewConfig};
use iroh_topic_tracker::TopicDiscoveryHook;
use sha2::Digest;
use tracing::{debug, error, info, trace, warn};

use crate::{
    ConnState, Direct, DirectMessage, Router, Tun, local_networking::Ipv4Pkg, router::RouterIp,
};

const PENDING_TTL: Duration = Duration::from_secs(60);
const PENDING_MAX_PER_IP: usize = 1024 * 16;

#[derive(Debug, Clone)]
pub struct Network {
    api: Handle<NetworkActor, anyhow::Error>,
}

#[derive(Debug)]
struct NetworkActor {
    router: Router,
    direct: Direct,
    _auth: Authenticator,

    _iroh_router: iroh::protocol::Router,
    //_docs_router: iroh::protocol::Router,
    iroh_endpoint: iroh::endpoint::Endpoint,

    tun: Option<Tun>,

    tun_ip_debug: Option<std::net::Ipv4Addr>,

    _local_to_direct_tx: tokio::sync::mpsc::Sender<Ipv4Pkg>,
    local_to_direct_rx: tokio::sync::mpsc::Receiver<Ipv4Pkg>,

    _direct_to_local_tx: tokio::sync::mpsc::Sender<DirectMessage>,
    direct_to_local_rx: tokio::sync::mpsc::Receiver<DirectMessage>,

    ip_cache: HashMap<std::net::Ipv4Addr, EndpointId>,
    peer_ids: HashSet<EndpointId>,
    pending_packets: HashMap<std::net::Ipv4Addr, VecDeque<(Instant, Ipv4Pkg)>>,
}

/*fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(10_000))))
        .keep_alive_interval(Duration::from_millis(20_000))
        .build()
}*/

fn transport_config(log_path: Option<PathBuf>) -> QuicTransportConfig {
    const EXPECTED_RTT: u32 = 100;
    //const MAX_STREAM_BANDWIDTH: u32 = 512_000;
    const STREAM_RWND: u32 = 102_400;//MAX_STREAM_BANDWIDTH / 1000 * EXPECTED_RTT * 2;

    let mut transport = QuicTransportConfig::builder()
        //.congestion_controller_factory(Arc::new(BbrConfig::default()))
        .enable_segmentation_offload(false)
        .packet_threshold(3)
        .time_threshold(1.125)
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(45_000))))
        .keep_alive_interval(Duration::from_millis(3_000))
        .stream_receive_window(STREAM_RWND.into())
        .send_window((4 * STREAM_RWND).into())
        .initial_rtt(Duration::from_millis(EXPECTED_RTT as u64))
        .initial_mtu(1660)
        .min_mtu(1660)
        .datagram_receive_buffer_size(Some(65_536))
        .datagram_send_buffer_size(65_536);

    if let Some(log_path) = log_path {
        if !log_path.exists()
            && let Err(e) = std::fs::create_dir_all(&log_path)
        {
            error!("Failed to create qlog directory at {:?}: {}", log_path, e);
        }
        transport = transport.qlog_from_path(log_path, "iroh-lan");
    }

    transport.build()
}

/*
fn rustonbsd_relay() -> RelayConfig {
    let url: Url = format!("https://iroh-relay.rustonbsd.com/")
        .parse()
        .expect("default url");
    RelayConfig {
        url: url.into(),
        quic: Some(RelayQuicConfig::default()),
    }
} */

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DummyProtocol;
impl ProtocolHandler for DummyProtocol {
    async fn accept(&self, _conn: Connection) -> Result<(), iroh::protocol::AcceptError> {
        Ok(())
    }
}
impl Network {
    pub async fn new(name: &str, password: &str) -> Result<Self> {
        Self::new_logs_dir(name, password, None).await
    }
    pub async fn new_logs_dir(
        name: &str,
        password: &str,
        log_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let secret_key = SecretKey::generate();

        let mut network_secret = sha2::Sha512::new();
        network_secret.update(format!("iroh-lan-network-name-{}", name));
        network_secret.update(format!("iroh-lan-network-secret-{password}"));
        let network_secret: [u8; 64] = network_secret.finalize().into();

        let auth = Authenticator::new(&network_secret);
        let topic_discovery_hook = TopicDiscoveryHook::new();
        //let relay_map = RelayMap::from_iter([rustonbsd_relay()]);
        //relay_map.extend(&iroh::defaults::prod::default_relay_map());

        let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
            //.relay_mode(iroh::RelayMode::Custom(relay_map))
            .hooks(auth.clone())
            .hooks(topic_discovery_hook.clone())
            .secret_key(secret_key.clone())
            .transport_config(transport_config(log_dir))
            .bind()
            .await?;
        auth.set_endpoint(&endpoint).await;
        endpoint.online().await;

        let gossip_hyparview_config = HyparviewConfig {
            neighbor_request_timeout: Duration::from_millis(3000),
            shuffle_interval: Duration::from_secs(120),
            ..Default::default()
        };

        let gossip_plumtree_config = iroh_gossip::proto::PlumtreeConfig {
            message_cache_retention: Duration::from_secs(300),
            cache_evict_interval: Duration::from_secs(60),
            message_id_retention: Duration::from_secs(300),
            graft_timeout_1: Duration::from_secs(2),
            graft_timeout_2: Duration::from_secs(1),
            ..Default::default()
        };

        let gossip = Gossip::builder()
            .max_message_size(64 * 1024)
            .membership_config(gossip_hyparview_config)
            .broadcast_config(gossip_plumtree_config)
            .spawn(endpoint.clone());

        let (direct_connect_tx, direct_connect_rx) = tokio::sync::mpsc::channel(1024 * 16);
        let direct = Direct::new(endpoint.clone(), direct_connect_tx.clone());

        let _iroh_router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_auth::ALPN, auth.clone())
            .accept(crate::Direct::ALPN, direct.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();

        let router = crate::Router::builder(topic_discovery_hook)
            .entry_name(name)
            .password(password)
            .secret_key(secret_key)
            .endpoint(endpoint.clone())
            .gossip(gossip)
            .build()
            .await?;

        direct.set_router(router.clone()).await?;

        let (to_remote_writer, to_remote_reader) = tokio::sync::mpsc::channel(1024 * 16);
        let (api, _) = Handle::spawn_with(NetworkActor {
            router,
            direct,
            _auth: auth,

            _iroh_router,
            //_docs_router,
            iroh_endpoint: endpoint,

            tun: None,
            tun_ip_debug: None,
            _local_to_direct_tx: to_remote_writer,
            local_to_direct_rx: to_remote_reader,
            _direct_to_local_tx: direct_connect_tx,
            direct_to_local_rx: direct_connect_rx,

            ip_cache: HashMap::new(),
            peer_ids: HashSet::new(),
            pending_packets: HashMap::new(),
        }, |mut actor, rx| async move { actor.run(rx).await });

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

impl NetworkActor {
    async fn run(&mut self, rx: Receiver<Action<NetworkActor>>) -> Result<()> {
        let mut ip_tick = tokio::time::interval(Duration::from_millis(500));
        ip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut cache_tick = tokio::time::interval(Duration::from_secs(1));
        cache_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("NetworkActor started");

        loop {
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                   trace!("NetworkActor action received");
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl-C, shutting down NetworkActor");
                    break
                }

                // init tun after ip is assigned
                _ = ip_tick.tick() => {
                    if let Some(tun_ip) = self.tun_ip_debug
                        && let Ok(RouterIp::AssignedIp(router_ip)) = self.router.get_ip_state().await
                        && tun_ip != router_ip {
                            error!("IP Configuration Mismatch: TUN={}, Router={}. Connectivity compromised.", tun_ip, router_ip);
                        }
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
                                        self.tun_ip_debug = Some(ip);
                                        self.tun = Some(tun);
                                        if let Ok(peers) = self.router.get_peers().await {
                                            for (id, remote_ip) in peers {
                                                if remote_ip.is_some() {
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
                        let mut router_peers: HashMap<std::net::Ipv4Addr, EndpointId> = HashMap::new();

                        for (id, maybe_ip) in peers {
                            next_peer_ids.insert(id);
                            if let Some(ip) = maybe_ip {
                                router_peers.insert(ip, id);
                            }
                        }

                        let cached_ips: Vec<_> = self.ip_cache.keys().copied().collect();
                        for ip in cached_ips {
                            if let std::collections::hash_map::Entry::Vacant(e) = router_peers.entry(ip)
                                && let Some(owner_id) = self.ip_cache.get(&ip)
                                    && matches!(self.direct.get_peer_state(*owner_id).await, Ok(ConnState::Open) | Ok(ConnState::Connecting)) {
                                        debug!("[Data-Plane Liveness] Preserving route to {} (owned by {}) despite Router/Doc miss. Connection is OPEN.", ip, owner_id);
                                        e.insert(*owner_id);
                                        next_peer_ids.insert(*owner_id);
                                    }
                        }

                        self.ip_cache = router_peers;
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
                                if let Some(id) = self.ip_cache.get(&ip).copied() {
                                    let peer_state = self.direct.get_peer_state(id).await;
                                    if !matches!(peer_state, Ok(ConnState::Open)) {
                                        continue;
                                    }

                                    if let Some(mut queue) = self.pending_packets.remove(&ip) {
                                        let replay_count = queue.len();
                                        info!(
                                            "Replaying {} buffered packets for {} via {}",
                                            replay_count,
                                            ip,
                                            id
                                        );

                                        let mut failed_queue = VecDeque::new();
                                        while let Some((ts, pkt)) = queue.pop_front() {
                                            if let Err(e) = self
                                                .direct
                                                .route_packet(id, DirectMessage::IpPacket(pkt.clone()))
                                                .await
                                            {
                                                warn!(
                                                    "Failed to route buffered packet to {} for ip {} state={:?}: {}",
                                                    id,
                                                    ip,
                                                    peer_state,
                                                    e
                                                );
                                                failed_queue.push_back((ts, pkt));
                                                failed_queue.append(&mut queue);
                                                break;
                                            }
                                        }

                                        if !failed_queue.is_empty() {
                                            self.pending_packets.insert(ip, failed_queue);
                                        }
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
                                        if to == self.iroh_endpoint.id() {
                                            trace!("Loopback packet detected (dest to self)");
                                            if let Some(tun) = &self.tun
                                                && let Err(e) = tun.write(tun_recv.clone()).await {
                                                    warn!("Failed to loopback packet to TUN: {}", e);
                                                    self.queue_packet(dest, tun_recv);
                                                }
                                        } else {
                                            trace!("Routing packet from TUN to {}", to);
                                            if let Err(e) = self.direct.route_packet(to, DirectMessage::IpPacket(tun_recv.clone())).await {
                                                self.queue_packet(dest, tun_recv);
                                                trace!("Failed to route packet to {}: {}", to, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        trace!("Could not resolve endpoint for IP {}: {}", dest, e);
                                        self.queue_packet(dest, tun_recv);
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

impl NetworkActor {
    fn queue_packet(&mut self, ip: std::net::Ipv4Addr, pkt: Ipv4Pkg) {
        let queue = self.pending_packets.entry(ip).or_default();
        if queue.len() >= PENDING_MAX_PER_IP {
            queue.pop_front();
            warn!(
                "Buffered packet queue at cap for {}. Dropping oldest packet before enqueue",
                ip
            );
        }
        queue.push_back((Instant::now(), pkt.clone()));
        if queue.len() == 1 || queue.len().is_multiple_of(256) {
            info!(
                "Buffered packet queue for {} now has {} packets",
                ip,
                queue.len()
            );
        }
    }
}
