use std::{
    collections::HashMap,
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use distributed_topic_tracker::{
    AutoDiscoveryGossip, GossipReceiver, GossipSender, RecordPublisher, Topic, TopicId,
};
use iroh::NodeId;
use serde::{Deserialize, Serialize};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, SecretKey};
use iroh_gossip::{net::Gossip, proto::HyparviewConfig};
use tokio::{task::block_in_place, time::sleep};
use tun_rs::AsyncDevice;

use crate::{Direct, DirectMessage, local_networking::Ipv4Pkg};

pub struct Router {
    pub gossip_sender: GossipSender,
    pub gossip_receiver: GossipReceiver,
    router_requester: tokio::sync::mpsc::Sender<RouterRequest>,
    _router: iroh::protocol::Router,
    pub node_id: NodeId,
    _topic: Option<Topic>,
    pub direct: Arc<Direct>,
    pub direct_connect_sender: tokio::sync::broadcast::Sender<DirectMessage>,
    pub _keep_alive_direct_connect_reader: tokio::sync::broadcast::Receiver<DirectMessage>,
    pub tun: Option<crate::Tun>,
}

impl Clone for Router {
    fn clone(&self) -> Self {
        Self {
            gossip_sender: self.gossip_sender.clone(),
            gossip_receiver: self.gossip_receiver.clone(),
            _router: self._router.clone(),
            router_requester: self.router_requester.clone(),
            node_id: self.node_id.clone(),
            _topic: self._topic.clone(),
            direct: Arc::clone(&self.direct),
            direct_connect_sender: self.direct_connect_sender.clone(),
            _keep_alive_direct_connect_reader: self.direct_connect_sender.subscribe(),
            tun: self.tun.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMessage {
    node_id_ip_dict: HashMap<NodeId, Ipv4Addr>,
    timestamp: i64,
    leader: Option<NodeId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqMessage {
    node_id: NodeId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum RouterMessage {
    StateMessage(StateMessage),
    ReqMessage(ReqMessage),
}

#[derive(Debug)]
enum RouterRequest {
    GetRouterState(tokio::sync::oneshot::Sender<RouterState>),
    SetNodeIdIpDict(HashMap<NodeId, Ipv4Addr>, tokio::sync::oneshot::Sender<()>),
    SetLeader(NodeId, tokio::sync::oneshot::Sender<()>),
    SetLastLeaderMsg(StateMessage, tokio::sync::oneshot::Sender<()>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterState {
    pub node_id_ip_dict: HashMap<NodeId, Ipv4Addr>,
    pub leader: Option<NodeId>,
    pub last_leader_msg: Option<StateMessage>,
}

#[derive(Debug, Clone)]
pub struct Builder {
    entry_name: String,
    secret_key: SecretKey,
    creator_mode: bool,
    password: String,
    tun: Option<crate::Tun>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    pub fn entry_name(mut self, entry_name: &str) -> Self {
        self.entry_name = entry_name.to_string();
        self
    }

    pub fn secret_key(mut self, secret_key: SecretKey) -> Self {
        self.secret_key = secret_key;
        self
    }

    pub fn creator_mode(mut self) -> Self {
        self.creator_mode = true;
        self
    }

    pub fn password(mut self, password: &str) -> Self {
        self.password = password.to_string();
        self
    }

    pub fn tun(mut self, tun: crate::Tun) -> Self {
        self.tun = Some(tun);
        self
    }

    pub async fn build(&self) -> Result<Router> {
        let endpoint = Endpoint::builder()
            .discovery_n0()
            .secret_key(self.secret_key.clone())
            .bind()
            .await?;

        let gossip_config = HyparviewConfig {
            neighbor_request_timeout: Duration::from_millis(2000),
            ..Default::default()
        };

        let gossip = Gossip::builder()
            .membership_config(gossip_config)
            .spawn(endpoint.clone());

        let (direct_connect_tx, _direct_connect_rx) = tokio::sync::broadcast::channel(1024 * 16);
        let direct = Direct::new(endpoint.clone(), direct_connect_tx.clone());

        let _router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(crate::Direct::ALPN, direct.clone())
            .spawn();

        let topic_initials = format!("lanparty-{}", self.entry_name);
        let secret_initials = format!("{topic_initials}-{}-secret", self.password)
            .as_bytes()
            .to_vec();

        let record_publisher = RecordPublisher::new(
            TopicId::new(topic_initials),
            endpoint.node_id(),
            self.secret_key.secret().clone(),
            None,
            secret_initials,
        );
        let topic = if self.creator_mode {
            gossip
                .subscribe_and_join_with_auto_discovery_no_wait(record_publisher)
                .await?
        } else {
            gossip
                .subscribe_and_join_with_auto_discovery(record_publisher)
                .await?
        };

        let (gossip_sender, gossip_receiver) = topic.split().await?;

        let (router_state_sender, router_state_reader) = tokio::sync::mpsc::channel(1024 * 16);

        let router_state_hs = if self.creator_mode {
            let mut hs = HashMap::<NodeId, Ipv4Addr>::new();
            hs.insert(endpoint.node_id(), Ipv4Addr::new(172, 22, 0, 2));
            hs
        } else {
            HashMap::<NodeId, Ipv4Addr>::new()
        };
        let mut router_state = RouterState {
            node_id_ip_dict: router_state_hs,
            leader: if self.creator_mode {
                Some(endpoint.node_id())
            } else {
                None
            },
            last_leader_msg: None,
        };

        tokio::spawn({
            async move {
                router_state.spawn(router_state_reader).await;
            }
        });
        

        let mut router = Router {
            gossip_sender,
            gossip_receiver,
            router_requester: router_state_sender,
            node_id: endpoint.node_id(),
            _topic: Some(topic),
            direct: Arc::new(direct),
            _keep_alive_direct_connect_reader: direct_connect_tx.subscribe(),
            direct_connect_sender: direct_connect_tx,
            _router,
            tun: None,
        };

        tokio::spawn({
            let router = router.clone();
            async move {
                let _ = router.spawn().await;
            }
        });

        while router.my_ip().await.is_none() {
            println!("Waiting to get an IP address...");
            sleep(Duration::from_secs(1)).await;
        }

        let my_ip = router.my_ip().await.ok_or_else(|| anyhow::anyhow!("failed to get my IP"))?;
        println!("My IP address is {}", my_ip);

        let (remote_writer, _remote_reader) = tokio::sync::broadcast::channel(1024*16);
        let tun = crate::Tun::new(
            (my_ip.octets()[2], my_ip.octets()[3]),
            remote_writer,
        )?;

        router.set_tun(tun).await?;

        if !self.creator_mode {
            sleep(Duration::from_millis(1000)).await;
            let data = postcard::to_stdvec(&RouterMessage::ReqMessage(ReqMessage {
                node_id: endpoint.node_id(),
            }))?;
            router.gossip_sender.broadcast(data).await?;
        }

        Ok(router)
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            creator_mode: false,
            entry_name: String::default(),
            secret_key: SecretKey::generate(&mut rand::thread_rng()),
            password: String::default(),
            tun: None,
        }
    }
}

impl Router {
    pub fn builder() -> Builder {
        Builder::new()
    }

    pub async fn close(self) -> Result<()> {
        self.tun.context("failed to get tun device")?.close().await
    }

    pub async fn set_tun(&mut self, tun: crate::Tun) -> Result<()> {
        self.tun = Some(tun);
        Ok(())
    }

    async fn spawn(&self) -> Result<()> {
        println!("router.spawn");
        while let Some(Ok(event)) = self.gossip_receiver.next().await {
            if let iroh_gossip::api::Event::Received(message) = event {
                if let Ok(router_msg) = postcard::from_bytes(message.content.to_vec().as_slice()) {
                    match router_msg {
                        RouterMessage::StateMessage(state_message) => {
                            if let Ok(state) = self.get_state().await {
                                let _ = self.set_last_leader_msg(state_message.clone()).await;
                                let _ = self
                                    .set_node_id_ip_dict(state_message.node_id_ip_dict)
                                    .await;
                                if state.leader.is_none()
                                    || (state_message.leader.is_some()
                                        && state.leader.unwrap() != state_message.leader.unwrap())
                                {
                                    let _ = self.set_leader(state_message.leader.unwrap()).await;
                                }
                            }
                        }
                        RouterMessage::ReqMessage(req_message) => {
                            if let Ok(state) = self.get_state().await {
                                if let Some(leader) = state.leader {
                                    if leader == self.node_id {
                                        if let Ok(next_ip) = self.get_next_ip().await {
                                            if self
                                                .add_node_id_ip(req_message.node_id, next_ip)
                                                .await
                                                .is_ok()
                                            {
                                                if let Ok(state) = self.get_state().await {
                                                    let msg =
                                                        RouterMessage::StateMessage(StateMessage {
                                                            node_id_ip_dict: state.node_id_ip_dict,
                                                            timestamp: SystemTime::now()
                                                                .duration_since(
                                                                    SystemTime::UNIX_EPOCH,
                                                                )
                                                                .unwrap_or(Duration::from_secs(0))
                                                                .as_secs()
                                                                as i64,
                                                            leader: state.leader,
                                                        });
                                                    let data = postcard::to_stdvec(&msg)
                                                        .expect("serialization failed");
                                                    let _ =
                                                        self.gossip_sender.broadcast(data).await;
                                                }
                                            }
                                        } else {
                                            //println!("get next ip failed");
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    //println!("Failed to deserialize gossip message");
                }
            }
        }

        //println!("Failed!!!!");
        Ok(())
    }

    async fn get_next_ip(&self) -> Result<Ipv4Addr> {
        let state = self.get_state().await?;
        let &highest_ip = state
            .node_id_ip_dict
            .values()
            .max_by_key(|&ip| ip.octets()[2] as u16 * 256u16 + ip.octets()[3] as u16)
            .unwrap_or(&Ipv4Addr::new(172, 22, 0, 3));

        let next_ip = Ipv4Addr::new(
            172,
            22,
            if highest_ip.octets()[3] == 255 { 1 } else { 0 } + highest_ip.octets()[2],
            if highest_ip.octets()[3] == 255 {
                0
            } else {
                highest_ip.octets()[3] + 1
            },
        );

        // to avoid overflow and therfore dublicates we check if this ip is already contained
        if state.node_id_ip_dict.values().any(|v| v.eq(&next_ip)) {
            bail!("invalid ip")
        }

        Ok(next_ip)
    }

    async fn add_node_id_ip(&self, node_id: NodeId, ip: Ipv4Addr) -> Result<()> {
        let mut state = self.get_state().await?;
        let _ = state.node_id_ip_dict.insert(node_id, ip);
        self.set_node_id_ip_dict(state.node_id_ip_dict).await?;
        Ok(())
    }

    async fn set_last_leader_msg(&self, latest_leader_msg: StateMessage) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.router_requester
            .send(RouterRequest::SetLastLeaderMsg(latest_leader_msg, tx))
            .await?;
        rx.await?;
        Ok(())
    }

    async fn set_leader(&self, leader: NodeId) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.router_requester
            .send(RouterRequest::SetLeader(leader, tx))
            .await?;
        rx.await?;
        Ok(())
    }

    async fn set_node_id_ip_dict(&self, node_id_ip_dict: HashMap<NodeId, Ipv4Addr>) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.router_requester
            .send(RouterRequest::SetNodeIdIpDict(node_id_ip_dict, tx))
            .await?;
        rx.await?;
        Ok(())
    }

    pub async fn get_state(&self) -> Result<RouterState> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.router_requester
            .send(RouterRequest::GetRouterState(tx))
            .await?;
        Ok(rx.await?)
    }

    // Returns the NodeId of the destination ipv4
    pub async fn ip_to_node_id(&self, pkg: Ipv4Pkg) -> Result<NodeId> {
        let pkg = pkg.to_ipv4_packet()?;
        let dest = pkg.get_destination();

        let state = self.get_state().await?;
        let (dest_node_id, _) = state
            .node_id_ip_dict
            .iter()
            .filter(|(_, ip)| ip.octets() == dest.octets())
            .next()
            .context("")?;

        Ok(*dest_node_id)
    }

    pub async fn node_id_to_ip(&self, node_id: NodeId) -> Result<Ipv4Addr> {
        let state = self.get_state().await?;
        let ip = state
            .node_id_ip_dict
            .get(&node_id)
            .context("node id not found")?;
        Ok(*ip)
    }

    pub fn subscribe_direct_connect(&self) -> tokio::sync::broadcast::Receiver<DirectMessage> {
        self.direct_connect_sender.subscribe()
    }

    pub fn my_node_id(&self) -> NodeId {
        self.node_id
    }

    pub async fn my_ip(&self) -> Option<Ipv4Addr> {
        if let Ok(ip) = self.node_id_to_ip(self.my_node_id()).await {
            Some(ip)
        } else {
            if let Ok(data) = postcard::to_stdvec(&RouterMessage::ReqMessage(ReqMessage {
                node_id: self.node_id,
            })) {
                let _ = self.gossip_sender.broadcast(data).await;
            }
            None
        }
    }

    pub async fn get_leader(&self) -> Result<Option<NodeId>> {
        let state = self.get_state().await?;
        Ok(state.leader)
    }
}

impl RouterState {
    async fn spawn(&mut self, reader: tokio::sync::mpsc::Receiver<RouterRequest>) {
        let mut reader = reader;
        loop {
            tokio::select! {
                Some(router_request) = reader.recv() => match router_request {
                    RouterRequest::GetRouterState(sender)=>{
                        let _ = sender.send(self.clone());
                    },
                    RouterRequest::SetNodeIdIpDict(hash_map, sender) => {
                        self.node_id_ip_dict = hash_map;
                        let _ = sender.send(());
                    },
                    RouterRequest::SetLeader(public_key, sender) => {
                        self.leader = Some(public_key);
                        let _ = sender.send(());
                    },
                    RouterRequest::SetLastLeaderMsg(router_message, sender) => {
                        self.last_leader_msg = Some(router_message);
                        let _ = sender.send(());
                    },
                }
            }
        }
    }
}
