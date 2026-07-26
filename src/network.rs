use std::{
    collections::HashMap,
    io::BufWriter,
    net::Ipv4Addr,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use actor_helper::{Action, Handle, Receiver, act, act_ok};
use anyhow::Result;
use ipnet::Ipv4Net;
use iroh::{
    Endpoint, EndpointId, RelayMap, RelayUrl, SecretKey,
    endpoint::{IdleTimeout, QlogConfig, QlogFactory, QuicTransportConfig, VarInt},
};
use iroh_auth::Authenticator;

use iroh_gossip::{net::Gossip, proto::HyparviewConfig};
use noq_proto::ConnectionId;
use sha2::Digest;
use tracing::{debug, error, info, trace, warn};

use crate::{
    Direct, DirectMessage, Overlay, Tun,
    state::{Config, NodeState},
    tun::{Ipv4Pkg, TunEvent},
};

#[derive(Debug)]
struct LoggingQlogFactory {
    dir: PathBuf,
    prefix: String,
}

impl LoggingQlogFactory {
    fn new(dir: PathBuf, prefix: impl Into<String>) -> Self {
        Self {
            dir,
            prefix: prefix.into(),
        }
    }
}

impl QlogFactory for LoggingQlogFactory {
    fn for_connection(
        &self,
        side: iroh::endpoint::Side,
        remote: std::net::SocketAddr,
        initial_dst_cid: ConnectionId,
        now: Instant,
    ) -> Option<QlogConfig> {
        let timestamp = std::time::SystemTime::now()
            .checked_sub(Instant::now().duration_since(now))?
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis();
        let side_text = format!("{side:?}").to_lowercase();
        let prefix = if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}-", self.prefix)
        };
        let file_name = format!("{prefix}{timestamp}-{initial_dst_cid}-{side_text}.qlog");
        let path = self.dir.join(file_name);
        let file = std::fs::File::create(&path)
            .inspect_err(|err| warn!("Failed to create qlog file at {}: {}", path.display(), err))
            .ok()?;

        info!(
            "iroh-qlog-created remote_addr={} side={} initial_dst_cid={} path={}",
            remote,
            side_text,
            initial_dst_cid,
            path.display()
        );

        Some(QlogConfig::new(Box::new(BufWriter::new(file))))
    }
}

#[derive(Debug, Clone)]
pub struct Network {
    api: Handle<NetworkActor, anyhow::Error>,
}

#[derive(Debug)]
struct NetworkActor {
    overlay: Overlay,
    direct: Direct,
    _auth: Authenticator,

    _iroh_router: iroh::protocol::Router,
    iroh_endpoint: iroh::endpoint::Endpoint,

    tun: Option<Tun>,
    tun_config_rx: tokio::sync::mpsc::Receiver<TunEvent>,

    tun_ip_debug: Option<std::net::Ipv4Addr>,

    tun_to_remote_tx: tokio::sync::mpsc::Sender<Ipv4Pkg>,
    tun_to_remote_rx: tokio::sync::mpsc::Receiver<Ipv4Pkg>,

    remote_to_tun_rx: tokio::sync::mpsc::Receiver<DirectMessage>,

    eid_ip_mapping_update_rx: tokio::sync::mpsc::Receiver<(EndpointId, Option<Ipv4Addr>)>,

    ip_cache: HashMap<std::net::Ipv4Addr, EndpointId>,
}

fn transport_config(log_path: Option<PathBuf>) -> QuicTransportConfig {
    let mut transport = QuicTransportConfig::builder()
        .enable_segmentation_offload(false)
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(10_000))))
        //.keep_alive_interval(Duration::from_millis(1_000))
        .initial_mtu(1200)
        //.mtu_discovery_config(None)
        .min_mtu(1200);

    if let Some(log_path) = log_path {
        if !log_path.exists()
            && let Err(e) = std::fs::create_dir_all(&log_path)
        {
            error!("Failed to create qlog directory at {:?}: {}", log_path, e);
        }
        transport = transport.qlog_factory(Arc::new(LoggingQlogFactory::new(log_path, "iroh-lan")));
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

impl Network {
    pub async fn new(
        name: &str,
        password: &str,
        relay_url: Option<String>,
        config: Config,
    ) -> Result<Self> {
        Self::new_logs_dir(name, password, None, relay_url, config).await
    }
    pub async fn new_logs_dir(
        name: &str,
        password: &str,
        log_dir: Option<PathBuf>,
        relay_url: Option<String>,
        config: Config,
    ) -> Result<Self> {
        let secret_key = SecretKey::generate();

        let mut network_secret = sha2::Sha512::new();
        network_secret.update(format!("iroh-lan-network-name-{}", name));
        network_secret.update(format!("iroh-lan-network-secret-{password}"));
        let network_secret: [u8; 64] = network_secret.finalize().into();

        let auth = Authenticator::new(&network_secret);
        //let relay_map = RelayMap::from_iter([rustonbsd_relay()]);
        //relay_map.extend(&iroh::defaults::prod::default_relay_map());

        netwatch::add_ipnet_exclusion(ipnet::IpNet::V4(Ipv4Net::new(
            Ipv4Addr::from_octets([
                ((config.net_prefix & 0xFF00) >> 8) as u8,
                (config.net_prefix & 0x00FF) as u8,
                0,
                0,
            ]),
            16,
        )?));

        let mut endpoint_builder = Endpoint::builder(iroh::endpoint::presets::N0)
            //.relay_mode(iroh::RelayMode::Custom(relay_map))
            .hooks(auth.clone())
            .secret_key(secret_key.clone())
            .transport_config(transport_config(log_dir));

        if let Some(relay_url) = relay_url {
            endpoint_builder = endpoint_builder.relay_mode(iroh::RelayMode::Custom(
                RelayMap::from(RelayUrl::from_str(&relay_url)?),
            ));
            info!("Using custom relay URL: {}", relay_url);
        }

        let endpoint = endpoint_builder.bind().await?;

        auth.set_endpoint(&endpoint).await;
        endpoint.online().await;
        let socks = endpoint.bound_sockets();
        info!(
            "Endpoint online with ID: {}. Bound sockets: {:?}",
            endpoint.id(),
            socks
        );

        let gossip_hyparview_config = HyparviewConfig {
            //neighbor_request_timeout: Duration::from_millis(3000),
            //shuffle_interval: Duration::from_secs(120),
            ..Default::default()
        };

        let gossip_plumtree_config = iroh_gossip::proto::PlumtreeConfig {
            message_cache_retention: Duration::from_secs(300),
            cache_evict_interval: Duration::from_secs(60),
            message_id_retention: Duration::from_secs(300),
            ..Default::default()
        };

        let gossip = Gossip::builder()
            .max_message_size(64 * 1024)
            .membership_config(gossip_hyparview_config)
            .broadcast_config(gossip_plumtree_config)
            .spawn(endpoint.clone());

        // Pipes
        let (remote_to_tun_tx, remote_to_tun_rx) = tokio::sync::mpsc::channel(1024 * 16);
        let (tun_to_remote_tx, tun_to_remote_rx) = tokio::sync::mpsc::channel(1024 * 16);
        let (remote_frame_tx, remote_frame_rx) = tokio::sync::mpsc::channel(1024);
        let (transmit_frame_tx, transmit_frame_rx) = tokio::sync::mpsc::channel(1024);
        let (tun_config_tx, tun_config_rx) = tokio::sync::mpsc::channel(1024);
        let (eid_ip_mapping_update_tx, eid_ip_mapping_update_rx) = tokio::sync::mpsc::channel(1024);

        let direct = Direct::new(endpoint.clone(), remote_to_tun_tx, transmit_frame_rx, remote_frame_tx);

        let _iroh_router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_auth::ALPN, auth.clone())
            .accept(crate::Direct::ALPN, direct.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();

        let overlay = crate::Overlay::builder(
            endpoint.clone(),
            gossip,
            remote_frame_rx,
            transmit_frame_tx,
            tun_config_tx,
            eid_ip_mapping_update_tx,
        )
        .network_name(name)
        .password(password)
        .build()
        .await?;

        let (api, _) = Handle::spawn_with(
            NetworkActor {
                overlay,
                direct,
                _auth: auth,

                _iroh_router,
                iroh_endpoint: endpoint,

                tun: None,
                tun_config_rx,

                tun_ip_debug: None,
                tun_to_remote_tx,
                tun_to_remote_rx,
                remote_to_tun_rx,

                eid_ip_mapping_update_rx,

                ip_cache: HashMap::new(),
            },
            |mut actor, rx| async move { actor.run(rx).await },
        );

        Ok(Self { api })
    }

    pub async fn get_router_handle(&self) -> Result<Overlay> {
        self.api
            .call(act_ok!(actor => async move { actor.overlay.clone() }))
            .await
    }

    pub async fn get_node_id(&self) -> Result<EndpointId> {
        self.api
            .call(act!(actor => actor.overlay.get_endpoint_id()))
            .await
    }

    pub async fn get_peers(&self) -> Result<Vec<(EndpointId, Option<std::net::Ipv4Addr>)>> {
        self.api
            .call(act!(actor => actor.overlay.get_peers()))
            .await
    }

    pub async fn get_direct_handle(&self) -> Result<Direct> {
        self.api
            .call(act_ok!(actor => async move { actor.direct.clone() }))
            .await
    }

    pub async fn get_own_state(&self) -> Result<NodeState> {
        self.api
            .call(act!(actor => async move {
                actor.overlay.get_own_state().await
            }))
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
        debug!("NetworkActor started");

        loop {
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                   trace!("NetworkActor action received");
                    action(self).await;
                }

                Some(tun_event) = self.tun_config_rx.recv() => {
                    match tun_event {
                        TunEvent::Configure { ip } => {
                            println!("My IP is {}", ip);
                            self.create_tun(ip).await;
                        }
                        TunEvent::Reconfigure { to, .. } => {
                            if let Some(tun) = self.tun.take()
                                && let Err(err) = tun.close().await {
                                    warn!(?err, "failed to close TUN for reconfigure");
                                }
                            self.create_tun(to).await;
                        },
                        TunEvent::Teardown => {
                            if let Some(tun) = self.tun.take()
                                && let Err(err) = tun.close().await {
                                    warn!(?err, "failed to close TUN for reconfigure");
                                }
                        }
                    }
                }

                Some((peer, ip_event)) = self.eid_ip_mapping_update_rx.recv() => {
                    info!(%peer, "peer ip update");
                    if peer == self.iroh_endpoint.id() {
                        continue;
                    }
                    if let Some(ip) = ip_event {
                        self.ip_cache.insert(ip, peer);
                    }
                    self.direct.ensure_connection(peer).await?;
                }

                Some(tun_to_remote) = self.tun_to_remote_rx.recv(), if self.tun.is_some() => {
                    if let Ok(ip_pkg) = tun_to_remote.to_ipv4_packet() {
                        let dest = ip_pkg.get_destination();
                        if let Ok(Some(to)) = match self.ip_cache.get(&dest) {
                            Some(peer) => Ok(Some(*peer)),
                            None => self.overlay.get_endpoint_id_from_ip(dest).await,
                        }
                            && to != self.iroh_endpoint.id() {
                                trace!(%to, "routing packet from TUN");
                                if let Err(err) = self.direct.route_packet(to, DirectMessage::IpPacket(tun_to_remote)).await {
                                    warn!(%to, ?err, "failed to route packet");
                                }
                            }

                    }
                }

                Some(remote_to_tun) = self.remote_to_tun_rx.recv(), if self.tun.is_some() => {
                    if let Some(tun) = &self.tun && let DirectMessage::IpPacket(ip_pkg) = remote_to_tun {
                        trace!("Routing remote packet to TUN");
                        if let Err(err) = tun.write(ip_pkg).await {
                            warn!(?err,"Failed to write to TUN");
                        }
                    }
                }
                else => break,
            }
        }
        warn!("NetworkActor stopped");

        Ok(())
    }

    async fn create_tun(&mut self, tun_ip: Ipv4Addr) {
        match tokio::time::timeout(
            Duration::from_secs(10),
            Tun::new(
                tun_ip,
                self.tun_to_remote_tx.clone(),
                self.iroh_endpoint.clone(),
            ),
        )
        .await
        {
            Ok(Ok(tun)) => {
                info!("TUN initialized successfully");
                self.tun_ip_debug = Some(tun_ip);
                self.tun = Some(tun);
            }
            Ok(Err(err)) => {
                error!(?err, "failed to initialize TUN");
            }
            Err(timeout) => {
                error!(?timeout, "timeout waiting for TUN to initialize");
            }
        }
    }
}