use anyhow::{Context, Result};
use pnet_packet::Packet;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use std::collections::{hash_map::Entry, HashMap};

use iroh::{endpoint::Connection, protocol::ProtocolHandler, NodeAddr, NodeId};

use crate::{actor::{Action, Handle}, api_methods, local_networking::Ipv4Pkg};

#[derive(Debug)]
pub struct Direct {
    api: Handle<Actor>,
}

#[derive(Debug)]
struct Actor {
    peers: HashMap<NodeId, PeerState>,
    endpoint: iroh::endpoint::Endpoint,
    connection_tasks: JoinSet<(NodeId, (iroh::endpoint::SendStream, iroh::endpoint::RecvStream), Result<()>)>,
    handle: tokio::sync::mpsc::Receiver<Action<Actor>>,
    direct_connect_tx: tokio::sync::broadcast::Sender<DirectMessage>,
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


api_methods!(Handle<Actor>, Actor, {
    fn handle_connection(conn: Connection) -> Result<()>;
    fn route_packet(to: NodeId, pkg: DirectMessage) -> Result<()>;
});


impl Direct {
    pub const ALPN: &[u8] = b"/iroh/lan-direct/1";
    pub fn new(endpoint: iroh::endpoint::Endpoint, direct_connect_tx: tokio::sync::broadcast::Sender<DirectMessage>) -> Self {

        let (api, rx) = Handle::<Actor>::channel(1024);
        let mut actor = Actor {
            peers: HashMap::new(),
            endpoint,
            connection_tasks: JoinSet::new(),
            handle: rx,
            direct_connect_tx,
        };
        tokio::spawn(async move { actor.run().await });
        Self { api }
    }
}

impl Actor {

    async fn run(&mut self) {
        loop {
            tokio::select! {
                // Handle API actions
                Some(action) = self.handle.recv() => {
                    action(self).await;
                }

                // Keep your other selects (connection_tasks, timers, etc.)
                Some(joined) = self.connection_tasks.join_next() => {
                    if let Some((node_id, _streams, _res)) = joined.ok() {
                        // handle completed connection loop if needed
                        let _ = node_id;
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

        // Spawn a task for this connection
        let direct_tx = self.direct_connect_tx.clone();
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
                                // handle frame locally without borrowing `self`
                                if let Ok(pkg) = serde_json::from_slice::<DirectMessage>(&frame_buf) {
                                    match pkg {
                                        DirectMessage::IpPacket(ip_pkg) => {
                                            println!("Received packet from {}: dest: {} source: {}", remote_node_id, ip_pkg.to_ipv4_packet().get_destination(), ip_pkg.to_ipv4_packet().get_source());
                                            let _ = direct_tx.send(DirectMessage::IpPacket(ip_pkg));
                                        }
                                    }
                                }
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

    async fn route_packet(&mut self, to: NodeId, pkg: DirectMessage) -> Result<()> {
        if let Some(peer) = self.peers.get_mut(&to) {
            println!("Routing packet to {:?}", to);
            match peer {
                PeerState::Active { active_send_tx, .. } => {
                    active_send_tx.send(pkg).await.map_err(|e| anyhow::anyhow!("Failed to send packet to peer: {}", e))?;
                }
                PeerState::Pending { node_id, queue  } => {
                    // reconnect and resend the packet
                    if self.reconnect(*node_id).await.is_err() {
                        return Err(anyhow::anyhow!("Failed to reconnect to peer"));
                    }

                    while let Some(pkg) = queue.pop() {
                        if self.route_packet(*node_id, pkg).await.is_err() {
                        }
                    }
                }
            }
        } else {
            // insert as pending and try to connect
            println!("No connection to {}, reconnecting", to);
            self.peers.insert(to, PeerState::Pending { node_id: to, queue: vec![pkg] });
            if self.reconnect(to).await.is_err() {
                return Err(anyhow::anyhow!("Failed to reconnect to peer"));
            }
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
