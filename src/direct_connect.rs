use anyhow::{Context, Result};
use pnet_packet::Packet;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use std::collections::{hash_map::Entry, HashMap};

use iroh::{endpoint::Connection, protocol::ProtocolHandler, NodeAddr, NodeId};

use crate::local_networking::Ipv4Pkg;

#[derive(Debug)]
pub struct Direct {
    api: tokio::sync::mpsc::Sender<Api>,
}

#[derive(Debug)]
struct Actor {
    peers: HashMap<NodeId, PeerState>,
    endpoint: iroh::endpoint::Endpoint,
    connection_tasks: JoinSet<(NodeId, (iroh::endpoint::SendStream, iroh::endpoint::RecvStream), Result<()>)>,
    api: tokio::sync::mpsc::Receiver<Api>,
    _api_tx: tokio::sync::mpsc::Sender<Api>,
    direct_connect_tx: tokio::sync::broadcast::Sender<DirectMessage>,
}

#[derive(Debug)]
enum Api {
    HandleConnection(Connection,tokio::sync::oneshot::Sender<Result<()>>),
    HandleFrame(NodeId, Vec<u8>, tokio::sync::oneshot::Sender<Result<()>>),
    RoutePacket(NodeId, DirectMessage, tokio::sync::oneshot::Sender<Result<()>>),
    Reconnect(NodeId, tokio::sync::oneshot::Sender<Result<()>>),
}

#[derive(Debug)]
struct Frame {
    len: u32,
    data: Vec<u8>,
}

#[derive(Debug, Serialize,Deserialize, Clone)]
pub enum DirectMessage {
    IpPacket(Ipv4Pkg),
}


type ConnId = usize;

#[derive(Debug)]
enum PeerState {
    Pending {
        node_id: NodeId,
        queue: Vec<DirectMessage>,
    },
    Active {
        node_id: NodeId,
        active_send_tx: tokio::sync::mpsc::Sender<DirectMessage>,
        active_conn_id: ConnId,
        other_conns: Vec<ConnId>,
    },
}

impl PeerState {
    fn accept_conn(
        &mut self,
        send_tx: tokio::sync::mpsc::Sender<DirectMessage>,
        conn_id: ConnId,
        node_id: NodeId,
    ) -> Vec<DirectMessage> {
        match self {
            PeerState::Pending { queue, node_id } => {
                let queue = std::mem::take(queue);
                *self = PeerState::Active {
                    active_send_tx: send_tx,
                    active_conn_id: conn_id,
                    other_conns: Vec::new(),
                    node_id: *node_id,
                };
                queue
            }
            PeerState::Active {
                active_send_tx,
                active_conn_id,
                other_conns,
                node_id,
            } => {
                // We already have an active connection. We keep the old connection intact,
                // but only use the new connection for sending from now on.
                // By dropping the `send_tx` of the old connection, the send loop part of
                // the `connection_loop` of the old connection will terminate, which will also
                // notify the peer that the old connection may be dropped.
                other_conns.push(*active_conn_id);
                *active_send_tx = send_tx;
                *active_conn_id = conn_id;
                Vec::new()
            }
        }
    }
}


impl Direct {
    pub const ALPN: &[u8] = b"/iroh/lan-direct/1";
    pub fn new(endpoint: iroh::endpoint::Endpoint, direct_connect_tx: tokio::sync::broadcast::Sender<DirectMessage>) -> Self {
        let (api_tx, api_rx) = tokio::sync::mpsc::channel(1024);
        let mut actor = Actor::new(api_tx.clone(), api_rx, endpoint, direct_connect_tx);

        tokio::spawn(async move {
            actor.spawn().await;
        });

        Self { api: api_tx }
    }

    pub async fn handle_connection(&self, conn: Connection) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.api.send(Api::HandleConnection(conn, tx)).await.is_err() {
            return Err(anyhow::anyhow!("Direct actor task has shut down"));
        }
        rx.await?
    }

    pub async fn route_packet(&self, to: NodeId, pkg: DirectMessage) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.api.send(Api::RoutePacket(to, pkg, tx)).await.is_err() {
            return Err(anyhow::anyhow!("Direct actor task has shut down"));
        }
        rx.await?
    }

}

impl Actor {

    pub fn new(api_tx: tokio::sync::mpsc::Sender<Api>, api_rx: tokio::sync::mpsc::Receiver<Api>, endpoint: iroh::endpoint::Endpoint, direct_connect_tx: tokio::sync::broadcast::Sender<DirectMessage>) -> Self {
        Self {
            peers: HashMap::new(),
            endpoint,
            connection_tasks: JoinSet::new(),
            api: api_rx,
            _api_tx: api_tx,
            direct_connect_tx,
        }
    }

    async fn spawn(&mut self) {
        loop {
            tokio::select! {
                Some(api) = self.api.recv() => {
                    match api {
                        Api::HandleConnection(conn,resp) => {
                            let res = self.handle_connection(conn).await;
                            let _ = resp.send(res);
                        },
                        Api::HandleFrame(node_id, frame_buf, resp) => {
                            let res = self.handle_frame(node_id, frame_buf).await;
                            let _ = resp.send(res);
                        },
                        Api::RoutePacket(to, pkg, resp) => {
                            let res = self.route_packet(to, pkg).await;
                            let _ = resp.send(res);
                        },
                        Api::Reconnect(node_id, resp) => {
                            let res = self.reconnect(node_id).await;
                            let _ = resp.send(res);
                        }
                    }
                }
            }
        }
    }

    async fn handle_connection(&mut self, conn: iroh::endpoint::Connection) -> Result<()> {

        let (send_tx, mut send_rx) = tokio::sync::mpsc::channel(1024);
        let conn_id = conn.stable_id();
        let remote_node_id = conn.remote_node_id()?;
        let mut conn = conn.accept_bi().await?;

        let _ = match self.peers.entry(remote_node_id.clone()) {
            Entry::Occupied(mut entry) => entry.get_mut().accept_conn(send_tx, conn_id, remote_node_id.clone()),
            Entry::Vacant(entry) => {
                entry.insert(PeerState::Active {
                    active_send_tx: send_tx,
                    active_conn_id: conn_id,
                    other_conns: Vec::new(),
                    node_id: remote_node_id.clone(),
                });
                Vec::new()
            }
        };

        let api_tx = self._api_tx.clone();
        // Spawn a task for this connection
        self.connection_tasks.spawn(
            async move {
                
                let mut frame_len_buf = [0u8; 4];
                loop {
                    tokio::select! {
                        Some(msg) = send_rx.recv() => {
                            match msg {
                                DirectMessage::IpPacket(pkg) => {
                                    let bytes = pkg.to_ipv4_packet().packet().to_vec();
                                    if let Err(_) = conn.0.write(&bytes).await {
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(Some(_)) = conn.1.read(&mut frame_len_buf) => {
                            let frame_len = u32::from_be_bytes(frame_len_buf);
                            let mut frame_buf = vec![0u8; frame_len as usize];
                            if let Ok(Some(_)) = conn.1.read(&mut frame_buf).await {
                                let (tx, rx) = tokio::sync::oneshot::channel();
                                let _ = api_tx.send(Api::HandleFrame(remote_node_id, frame_buf, tx)).await;
                                let _ = rx.await;
                            } else {
                                break;
                            }
                        }
                    }
                }
                (remote_node_id, conn, Ok(()))
            }
        );
        Ok(())
    }

    async fn handle_frame(&self, from: NodeId, frame_buf: Vec<u8>) -> Result<()> {
        let pkg = serde_json::from_slice::<DirectMessage>(&frame_buf)?;
        match pkg {
            DirectMessage::IpPacket(ip_pkg) => {
                println!("Received packet from {}: dest: {} source: {}", from, ip_pkg.to_ipv4_packet().get_destination(), ip_pkg.to_ipv4_packet().get_source());
                let _ = self.direct_connect_tx.send(DirectMessage::IpPacket(ip_pkg));
            }
        }

        Ok(())
    }

    async fn route_packet(&mut self, to: NodeId, pkg: DirectMessage) -> Result<()> {
        if let Some(peer) = self.peers.get_mut(&to) {
            match peer {
                PeerState::Active { active_send_tx, .. } => {
                    active_send_tx.send(pkg).await.map_err(|e| anyhow::anyhow!("Failed to send packet to peer: {}", e))?;
                }
                PeerState::Pending { node_id, queue  } => {
                    // reconnect and resend the packet
                    let (tx,rx) = tokio::sync::oneshot::channel();
                    if self._api_tx.send(Api::Reconnect(*node_id, tx)).await.is_err() || rx.await.is_err() {
                        return Err(anyhow::anyhow!("Failed to reconnect to peer"));
                    }

                    while let Some(pkg) = queue.pop() {
                        let (tx,rx) = tokio::sync::oneshot::channel();
                        if self._api_tx.send(Api::RoutePacket(*node_id, pkg, tx)).await.is_ok() {
                            let _ = rx.await;
                        }
                    }
                }
            }
        } else {
            return Err(anyhow::anyhow!("No such peer"));
        }
        Ok(())
    }

    async fn reconnect(&self, node_id: NodeId) -> Result<()> {
        if let Some(peer) = self.peers.get(&node_id) {
            match peer {
                PeerState::Pending { node_id, .. } => {
                    self.endpoint.connect(*node_id, Direct::ALPN).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl ProtocolHandler for Direct {
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        let _ = self.handle_connection(connection).await;
        Ok(())
    }
}
