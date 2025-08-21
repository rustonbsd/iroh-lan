use std::{collections::HashMap, net::Ipv4Addr, sync::Arc};

use distributed_topic_tracker::{AutoDiscoveryGossip, GossipReceiver, GossipSender, TopicId};
use iroh::{
    NodeId,
    protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use anyhow::Result;
use distributed_topic_tracker::{AutoDiscoveryBuilder, DefaultSecretRotation};
use iroh::{Endpoint, SecretKey};
use iroh_gossip::net::Gossip;

#[derive(Debug, Clone)]
pub struct Router {
    gossip_sender: GossipSender,
    gossip_receiver: GossipReceiver,
    router_requester: tokio::sync::broadcast::Sender<RouterRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMessage {
    node_id_ip_dict: HashMap<NodeId, Ipv4Addr>,
    timestamp: i64,
    leader: NodeId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqMessage{
    node_id: NodeId,
    ipv4: Ipv4Addr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum RouterMessage {
    StateMessage(StateMessage),
    ReqMessage(ReqMessage),
}

#[derive(Debug, Clone)]
enum RouterRequest {
    ReqGetRouterState(tokio::sync::broadcast::Sender<RouterState>),
    ReqSetNodeIdIpDict(
        HashMap<NodeId, Ipv4Addr>,
        tokio::sync::broadcast::Sender<()>,
    ),
    ReqSetLeader(NodeId, tokio::sync::broadcast::Sender<()>),
    ReqSetLastLeaderMsg(StateMessage, tokio::sync::broadcast::Sender<()>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouterState {
    node_id_ip_dict: HashMap<NodeId, Ipv4Addr>,
    leader: Option<NodeId>,
    last_leader_msg: Option<StateMessage>,
}

#[derive(Debug, Clone)]
pub struct Builder {
    entry_name: String,
    secret_key: SecretKey,

    // FIX UP BUILDER AND MAKE GOSSIP WORK WITH NODEID_IPV4 pairs
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    pub fn node_id_ip(&mut self, node_id: NodeId, ip: Ipv4Addr) -> Self {
        self.node
    }

    pub async fn build(&self) -> Result<Router> {
        let endpoint = Endpoint::builder()
            .discovery_n0()
            .secret_key(self.secret_key.clone())
            .bind()
            .await?;
        let gossip = Gossip::builder()
            .spawn_with_auto_discovery(endpoint.clone(), Some(DefaultSecretRotation))
            .await?;
        let _router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.gossip.clone());

        let topic_initials = format!("lanparty-{}", self.entry_name);
        let secret_initials = format!("{topic_initials}-secret").as_bytes().to_vec();
        let (gossip_sender, gossip_receiver) = gossip
            .subscribe_and_join_with_auto_discovery(TopicId::new(topic_initials), &secret_initials)
            .await?
            .split();

        let (router_state_sender, mut router_state_reader) = tokio::sync::broadcast::channel(1024);
        tokio::spawn(async move { while router_state_reader.recv().await.is_ok() {} });

        let mut router_state = RouterState {
            node_id_ip_dict: HashMap::<NodeId, Ipv4Addr>::new(),
            leader: None,
            last_leader_msg: None,
        };

        tokio::spawn({
            let sender = router_state_sender.clone();
            async move {
                router_state.spawn(sender.clone()).await;
            }
        });

        let router = Router {
            gossip_sender,
            gossip_receiver,
            router_requester: router_state_sender,
        };

        tokio::spawn({
            let router = router.clone();
            async move {
                let _ = router.spawn().await;
            }
        });

        Ok(router)
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            entry_name: String::default(),
            secret_key: SecretKey::generate(&mut rand::thread_rng()),
        }
    }
}

impl Router {
    pub fn builder() -> Builder {
        Builder::new()
    }

    async fn spawn(&self) -> Result<()> {
        let mut recv = self.gossip_receiver.clone();
        while let Ok(event) = recv.recv().await {
            if let iroh_gossip::api::Event::Received(message) = event {
                if let Ok(router_msg) =
                    serde_json::from_slice::<RouterMessage>(message.content.to_vec().as_slice())
                {
                    println!("{router_msg:?}");
                    match router_msg {
                        RouterMessage::StateMessage(state_message) => {
                            if let Ok(state) = self.get_state().await {
                                let _ = self.set_last_leader_msg(state_message.clone()).await;
                                if state.leader.is_none()
                                    || state.leader.expect("") != state_message.leader
                                {
                                    let _ = self.set_leader(state_message.leader).await;
                                }
                            }
                        }
                        RouterMessage::ReqMessage(req_message) => {
                            if self.add_node_id_ip(req_message.node_id, req_message.ipv4).await.is_ok() {
                                if let Ok(state) = self.get_state().await {
                                    let data = serde_json::to_vec(&state).expect("serialization failed");
                                    let _ = self.gossip_sender.broadcast(data).await;
                                    println!("123");
                                }
                            }
                        },
                    }
                }
            }
        }
        Ok(())
    }

    async fn add_node_id_ip(&self, node_id: NodeId, ip: Ipv4Addr) -> Result<()> {
        let mut state = self.get_state().await?;
        let _ = state.node_id_ip_dict.insert(node_id, ip);
        self.set_node_id_ip_dict(state.node_id_ip_dict).await?;

        Ok(())
    }

    async fn set_last_leader_msg(&self, latest_leader_msg: StateMessage) -> Result<()> {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        self.router_requester
            .send(RouterRequest::ReqSetLastLeaderMsg(latest_leader_msg, tx))?;
        rx.recv().await?;
        Ok(())
    }

    async fn set_leader(&self, leader: NodeId) -> Result<()> {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        self.router_requester
            .send(RouterRequest::ReqSetLeader(leader, tx))?;
        rx.recv().await?;
        Ok(())
    }

    async fn set_node_id_ip_dict(&self, node_id_ip_dict: HashMap<NodeId, Ipv4Addr>) -> Result<()> {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        self.router_requester
            .send(RouterRequest::ReqSetNodeIdIpDict(node_id_ip_dict, tx))?;
        rx.recv().await?;
        Ok(())
    }

    async fn get_state(&self) -> Result<RouterState> {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        self.router_requester
            .send(RouterRequest::ReqGetRouterState(tx))?;
        Ok(rx.recv().await?)
    }
}

impl RouterState {
    async fn spawn(&mut self, sender: tokio::sync::broadcast::Sender<RouterRequest>) {
        let mut reader = sender.subscribe();
        loop {
            tokio::select! {
                Ok(router_request) = reader.recv() => match router_request {
                    RouterRequest::ReqGetRouterState(sender)=>{
                        let _ = sender.send(self.clone());
                    },
                    RouterRequest::ReqSetNodeIdIpDict(hash_map, sender) => {
                        self.node_id_ip_dict = hash_map;
                        let _ = sender.send(());
                    },
                    RouterRequest::ReqSetLeader(public_key, sender) => {
                        self.leader = Some(public_key);
                        let _ = sender.send(());
                    },
                    RouterRequest::ReqSetLastLeaderMsg(router_message, sender) => {
                        self.last_leader_msg = Some(router_message);
                        let _ = sender.send(());
                    },
                }
            }
        }
    }
}
