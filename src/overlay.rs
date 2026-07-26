use std::{net::Ipv4Addr, time::Duration};

use futures::StreamExt;
use iroh_gossip::{
    api::{Event as GossipEvent, GossipReceiver, GossipSender},
    net::Gossip,
};
use iroh_topic_tracker::{TopicDiscoveryConfig, TopicDiscoveryExt, TopicDiscoveryHandle};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, info, warn};

use anyhow::Result;
use iroh::{Endpoint, EndpointId};

use actor_helper::{Action, Handle, Receiver as ActorReceiver, act_ok};

use crate::{
    build_ip, current_time,
    state::{Config, MeshEvent, NodeState, State, TunDirective},
    tun::TunEvent,
};

#[derive(Debug)]
pub struct Builder {
    endpoint: Endpoint,
    gossip: Gossip,
    remote_frame_rx: Option<Receiver<(EndpointId, Vec<u8>)>>,
    transmit_frame_tx: Sender<(EndpointId, Vec<u8>)>,
    tun_config_tx: Sender<TunEvent>,
    eid_ip_mapping_update_tx: Sender<(EndpointId, Option<Ipv4Addr>)>,
    network_name: String,
    password: String,
    state_config: Config,
}

impl Builder {
    pub fn network_name(mut self, network_name: &str) -> Self {
        self.network_name = network_name.to_string();
        self
    }

    pub fn password(mut self, password: &str) -> Self {
        self.password = password.to_string();
        self
    }

    pub fn config(mut self, config: Config) -> Self {
        self.state_config = config;
        self
    }

    pub async fn build(mut self) -> Result<Overlay> {
        let topic_initials = format!("lanparty-{}", self.network_name);
        let secret_initials = format!("{topic_initials}-{}-secret", self.password)
            .as_bytes()
            .to_vec();

        let mut topic_hasher = blake3::Hasher::new();
        topic_hasher.update(b"iroh-lan-topic");
        topic_hasher.update(&secret_initials);
        let topic_hash = *topic_hasher.finalize().as_bytes();

        let topic_discovery_config = TopicDiscoveryConfig::builder(self.endpoint.clone())
            .connection_timeout(Duration::from_secs(30))
            .announce_interval(Duration::from_secs(15 * 60))
            .first_connected_duration(Some(Duration::from_secs(60)))
            .discovery_interval_first_connected(Duration::from_secs(4))
            .dht_retries(None)
            .build();
        let (gossip_sender, gossip_receiver, topic_handle) = loop {
            if let Ok((gossip_sender, gossip_receiver, topic_handle)) = self
                .gossip
                .subscribe_with_discovery_joined(
                    topic_hash.to_vec(),
                    vec![],
                    topic_discovery_config.clone(),
                )
                .await
            {
                break (gossip_sender, gossip_receiver, Some(topic_handle));
            } else {
                warn!("Failed to join topic; retrying in 2 second");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };

        info!("Joined topic with hash: {:x?}", topic_hash);

        let (api, _) = Handle::spawn_with(
            OverlayActor {
                _topic: topic_handle,
                state: State::new(self.endpoint.secret_key(), self.state_config.clone(), None),

                received_frame_rx: self
                    .remote_frame_rx
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("remote frame rx not set"))?,

                transmit_frame_tx: self.transmit_frame_tx.to_owned(),

                tun_config_tx: self.tun_config_tx.to_owned(),

                eid_ip_mapping_update_tx: self.eid_ip_mapping_update_tx.to_owned(),

                _gossip_sender: gossip_sender,
                gossip_receiver,
            },
            |mut actor, rx| async move { actor.run(rx).await },
        );
        Ok(Overlay { api })
    }
}

#[derive(Debug, Clone)]
pub struct Overlay {
    api: Handle<OverlayActor, anyhow::Error>,
}

#[derive(Debug)]
struct OverlayActor {
    _topic: Option<TopicDiscoveryHandle>,
    state: State,

    // channel for `DirectConnect` `Connection`'s to write their received frames to
    received_frame_rx: Receiver<(EndpointId, Vec<u8>)>,

    // write frames to `DirectConnect` that will inturn write them to the specified peer `Connection`
    transmit_frame_tx: Sender<(EndpointId, Vec<u8>)>,
    // [!] TODO wire this into direct and thorugh to connection
    tun_config_tx: Sender<TunEvent>,

    eid_ip_mapping_update_tx: Sender<(EndpointId, Option<Ipv4Addr>)>,

    _gossip_sender: GossipSender,
    gossip_receiver: GossipReceiver,
}

impl Overlay {
    pub fn builder(
        endpoint: Endpoint,
        gossip: Gossip,
        remote_frame_rx: Receiver<(EndpointId, Vec<u8>)>,
        transmit_frame_tx: Sender<(EndpointId, Vec<u8>)>,
        tun_config_tx: Sender<TunEvent>,
        eid_ip_mapping_update_tx: Sender<(EndpointId, Option<Ipv4Addr>)>,
    ) -> Builder {
        Builder {
            endpoint,
            gossip,
            remote_frame_rx: Some(remote_frame_rx),
            transmit_frame_tx,
            tun_config_tx,
            eid_ip_mapping_update_tx,
            network_name: String::default(),
            password: String::default(),
            state_config: Config::default(),
        }
    }

    pub async fn get_endpoint_id(&self) -> Result<EndpointId> {
        self.api
            .call(act_ok!(actor => async move {
                *actor.state.endpoint_id()
            }))
            .await
    }

    pub async fn get_ip_from_endpoint_id(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<Option<Ipv4Addr>> {
        self.api
            .call(act_ok!(actor => async move {
                actor.state.get_node_states()
                    .iter()
                    .find(|state| state.endpoint_id == endpoint_id)
                    .and_then(|state| state.ip_claim.to_ipv4(actor.state.config().net_prefix))
            }))
            .await
    }

    pub async fn get_endpoint_id_from_ip(&self, ip: Ipv4Addr) -> Result<Option<EndpointId>> {
        self.api
            .call(act_ok!(actor => async move {
                actor.state.get_node_states()
                    .iter()
                    .filter_map(|state| {
                        if state.ip_claim.to_ipv4(actor.state.config().net_prefix) == Some(ip) {
                            Some(state.endpoint_id)
                        } else {
                            None
                        }
                    }).next()
            }))
            .await
    }

    pub async fn get_peers(&self) -> Result<Vec<(EndpointId, Option<Ipv4Addr>)>> {
        self.api
            .call(act_ok!(actor => async move {
                actor.state.get_node_states()
                    .iter()
                    .map(|state| (state.endpoint_id, state.ip_claim.to_ipv4(actor.state.config().net_prefix)))
                    .collect::<Vec<_>>()
            }))
            .await
    }

    pub async fn get_own_state(&self) -> Result<NodeState> {
        self.api
            .call(act_ok!(actor => async move { actor.state.own().clone() }))
            .await
    }
}

impl OverlayActor {
    async fn run(&mut self, rx: ActorReceiver<Action<OverlayActor>>) -> Result<()> {
        let mut ticker = tokio::time::interval(self.state.config().tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!(
            "RouterActor started. EndpointId: {}",
            self.state.endpoint_id()
        );
        loop {
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                    action(self).await;
                }
                _ = ticker.tick() => {
                    self.state.tick(current_time());
                    let _ =self.process_event().await;
                },
                Some((from, bytes)) = self.received_frame_rx.recv() => {
                    debug!(%from, "received frame");
                    if let Err(err) = self.state.on_frame(from, &bytes, current_time()) {
                        tracing::warn!(%from, ?err, "dropping bad frame");
                    }
                    let _ = self.process_event().await;
                },
                gossip_event = self.gossip_receiver.next() => {
                    match gossip_event {
                        Some(Ok(GossipEvent::NeighborUp(endpoint_id))) => {
                            self.state.on_peer_connected(endpoint_id, current_time());
                            let _ = self.process_event().await;
                            let _ = self.eid_ip_mapping_update_tx.send((endpoint_id, None)).await;
                        }
                        Some(Ok(GossipEvent::NeighborDown(endpoint_id))) => {
                            self.state.on_peer_disconnected(endpoint_id);
                            let _ =self.process_event().await;
                        }
                        Some(Ok(GossipEvent::Received(_))) => {
                            // we use keep alive packets for faster and more reliable state exchange
                        }
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            warn!(?err, "failed to read gossip event");
                        }
                        None => {
                            warn!("Gossip receiver closed");
                            break;
                        }
                    };
                }
                else => break,
            }
        }
        warn!("OverlayActor stopped.");
        Ok(())
    }

    async fn process_event(&mut self) -> Result<()> {
        for (to, frame) in self.state.drain_outbound() {
            if frame.is_empty() {
                println!("Process_Event: Sending empty frame");
            }
            if let Err(err) = self.transmit_frame_tx.send((to, frame)).await {
                warn!(%to, ?err, "failed to write frame");
            }
        }

        for event in self.state.drain_events() {
            match event {
                MeshEvent::Tun(TunDirective::Configure { ip }) => {
                    self.tun_config_tx
                        .send(TunEvent::Configure {
                            ip: build_ip(self.state.config().net_prefix, ip),
                        })
                        .await?;
                }
                MeshEvent::Tun(TunDirective::Reconfigure { from, to }) => {
                    tracing::info!(?from, ?to, "renumbering");
                    self.tun_config_tx
                        .send(TunEvent::Reconfigure {
                            from: build_ip(self.state.config().net_prefix, from),
                            to: build_ip(self.state.config().net_prefix, to),
                        })
                        .await?;
                }
                MeshEvent::Tun(TunDirective::Teardown { .. }) => {
                    self.tun_config_tx.send(TunEvent::Teardown).await?;
                }
                MeshEvent::PeerChanged { peer, state } => {
                    // update ip mapping
                    let ip_update = if let Some(ip) = state.ip_claim.ip() {
                        Some(build_ip(self.state.config().net_prefix, ip))
                    } else {
                        None
                    };
                    self.eid_ip_mapping_update_tx
                        .send((peer, ip_update))
                        .await?;

                    // update allowed ports
                    //todo!("update allowed ports");
                }
                MeshEvent::PeerDisconnected { peer } => {
                    info!(%peer, "peer disconnected (MeshEvent::PeerDisconnected)");
                }
                MeshEvent::ClaimResolved { ip, winner, loser } => {
                    tracing::info!(?ip, %winner, %loser, "claim resolved");
                }
            }
        }
        Ok(())
    }
}
