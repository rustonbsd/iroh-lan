use actor_helper::{Action, Handle, act};
use anyhow::Result;
use iroh::{NodeId, endpoint::Connection, protocol::ProtocolHandler};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, hash_map::Entry};
use tracing::{debug, info};

use crate::{connection::Conn, local_networking::Ipv4Pkg};

#[derive(Debug, Clone)]
pub struct Direct {
    api: Handle<DirectActor, anyhow::Error>,
}

#[derive(Debug)]
struct DirectActor {
    peers: HashMap<NodeId, Conn>,
    endpoint: iroh::endpoint::Endpoint,
    rx: actor_helper::Receiver<Action<DirectActor>>,
    direct_connect_tx: tokio::sync::broadcast::Sender<DirectMessage>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum DirectMessage {
    IpPacket(Ipv4Pkg),
    IDontLikeWarnings,
}

impl Direct {
    pub const ALPN: &[u8] = b"/iroh/lan-direct/1";
    pub fn new(
        endpoint: iroh::endpoint::Endpoint,
        direct_connect_tx: tokio::sync::broadcast::Sender<DirectMessage>,
    ) -> Self {
        let (api, rx) = Handle::channel();
        let mut actor = DirectActor {
            peers: HashMap::new(),
            endpoint,
            rx,
            direct_connect_tx,
        };
        tokio::spawn(async move { actor.run().await });
        Self { api }
    }

    pub async fn handle_connection(&self, conn: Connection) -> Result<()> {
        self.api
            .call(act!(actor => actor.handle_connection(conn)))
            .await
    }

    pub async fn route_packet(&self, to: NodeId, pkg: DirectMessage) -> Result<()> {
        self.api
            .call(act!(actor => actor.route_packet(to, pkg)))
            .await
    }

    pub async fn get_endpoint(&self) -> iroh::endpoint::Endpoint {
        self.api
            .call(act!(actor => actor.get_endpoint()))
            .await
            .unwrap()
    }

    pub async fn get_conn_state(&self, node_id: NodeId) -> Result<crate::connection::ConnState> {
        self.api
            .call(act!(actor => actor.get_conn_state(node_id)))
            .await
    }

    pub async fn close(&self) -> Result<()> {
        self.api.call(act!(actor => actor.close())).await
    }
}

impl DirectActor {
    async fn run(&mut self) {
        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    break
                }
            }
        }
    }

    async fn handle_connection(&mut self, conn: iroh::endpoint::Connection) -> Result<()> {
        info!("New direct connection from {:?}", conn.remote_node_id()?);
        let remote_node_id = conn.remote_node_id()?;

        match self.peers.entry(remote_node_id) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().incoming_connection(conn, true).await?;
            }
            Entry::Vacant(entry) => {
                let (send_stream, recv_stream) = conn.accept_bi().await?;
                entry.insert(
                    Conn::new(
                        self.endpoint.clone(),
                        conn,
                        send_stream,
                        recv_stream,
                        self.direct_connect_tx.clone(),
                    )
                    .await?,
                );
            }
        }

        Ok(())
    }

    async fn route_packet(&mut self, to: NodeId, pkg: DirectMessage) -> Result<()> {
        match self.peers.entry(to) {
            Entry::Occupied(entry) => {
                if entry.get().get_state().await == crate::connection::ConnState::Closed {
                    debug!("Connection to peer {} closed, removing", to);
                    entry.remove();
                    return Err(anyhow::anyhow!("connection to peer is not running"));
                }

                entry.get().write(pkg).await?;
            }
            Entry::Vacant(entry) => {
                let conn =
                    Conn::connect(self.endpoint.clone(), to, self.direct_connect_tx.clone()).await;

                conn.write(pkg).await?;
                entry.insert(conn);
            }
        }

        Ok(())
    }

    pub async fn get_conn_state(&self, node_id: NodeId) -> Result<crate::connection::ConnState> {
        Ok(self
            .peers
            .get(&node_id)
            .cloned()
            .ok_or(anyhow::anyhow!("no connection to peer"))?
            .get_state()
            .await)
    }

    pub async fn get_endpoint(&self) -> Result<iroh::endpoint::Endpoint> {
        Ok(self.endpoint.clone())
    }

    pub async fn close(&mut self) -> Result<()> {
        for (_, conn) in self.peers.drain() {
            let _ = conn.close().await;
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
