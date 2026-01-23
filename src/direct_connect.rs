use actor_helper::{Action, Handle, act};
use anyhow::Result;
use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::ProtocolHandler,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, hash_map::Entry};
use tracing::{debug, info, trace, warn, error};

use crate::{auth, connection::Conn, local_networking::Ipv4Pkg};

#[derive(Debug, Clone)]
pub struct Direct {
    api: Handle<DirectActor, anyhow::Error>,
}

#[derive(Debug)]
struct DirectActor {
    network_secret: [u8; 64],
    peers: HashMap<EndpointId, Conn>,
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
        network_secret: &[u8; 64],
    ) -> Self {
        let (api, rx) = Handle::channel();
        let mut actor = DirectActor {
            peers: HashMap::new(),
            endpoint,
            rx,
            direct_connect_tx,
            network_secret: *network_secret,
        };
        tokio::spawn(async move { actor.run().await });
        Self { api }
    }

    pub async fn handle_connection(&self, conn: Connection) -> Result<()> {
        self.api
            .call(act!(actor => actor.handle_connection(conn)))
            .await
    }

    pub async fn route_packet(&self, to: EndpointId, pkg: DirectMessage) -> Result<()> {
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

    pub async fn get_conn_state(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<crate::connection::ConnState> {
        self.api
            .call(act!(actor => actor.get_conn_state(endpoint_id)))
            .await
    }

    pub async fn close(&self) -> Result<()> {
        self.api.call(act!(actor => actor.close())).await
    }
}

impl DirectActor {
    async fn run(&mut self) -> Result<()> {
        debug!("DirectActor run loop started");
        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                     info!("Ctrl-C received, shutting down DirectActor");
                    break
                }
            }
        }
        Ok(())
    }

    async fn handle_connection(&mut self, conn: iroh::endpoint::Connection) -> Result<()> {
        info!("New direct connection from {:?}", conn.remote_id());
        let remote_id = conn.remote_id();

        match self.peers.entry(remote_id) {
            Entry::Occupied(mut entry) => {
                debug!("Existing connection found for {}, upgrading/resetting", remote_id);
                let (send_stream, recv_stream) =
                    auth::accept(&conn, &self.network_secret, self.endpoint.id(), remote_id)
                        .await?;
                entry
                    .get_mut()
                    .incoming_connection(conn, send_stream, recv_stream)
                    .await?;
            }
            Entry::Vacant(entry) => {
                debug!("New connection entry for {}", remote_id);
                let (send_stream, recv_stream) =
                    auth::accept(&conn, &self.network_secret, self.endpoint.id(), remote_id)
                        .await?;
                entry.insert(
                    Conn::new(
                        self.endpoint.clone(),
                        conn,
                        send_stream,
                        recv_stream,
                        self.direct_connect_tx.clone(),
                        &self.network_secret,
                    )
                    .await?,
                );
            }
        }

        Ok(())
    }

    async fn route_packet(&mut self, to: EndpointId, pkg: DirectMessage) -> Result<()> {
        trace!("Routing packet to {}", to);
        match self.peers.entry(to) {
            Entry::Occupied(entry) => {
                if entry.get().get_state().await == crate::connection::ConnState::Closed {
                    warn!("Connection to peer {} is closed, removing from peers", to);
                    entry.remove();
                    return Err(anyhow::anyhow!("connection to peer is not running"));
                }
                
                if let Err(e) = entry.get().write(pkg).await {
                     error!("Failed to write packet to peer {}: {:?}", to, e);
                     return Err(e);
                }
            }
            Entry::Vacant(entry) => {
                info!("No active connection to {}, initiating new connection", to);
                let conn = Conn::connect(
                    self.endpoint.clone(),
                    to,
                    self.direct_connect_tx.clone(),
                    &self.network_secret,
                )
                .await;

                if let Err(e) = conn.write(pkg).await {
                    error!("Failed to write packet to new connection {}: {:?}", to, e);
                    return Err(e);
                }
                entry.insert(conn);
            }
        }

        Ok(())
    }

    pub async fn get_conn_state(
        &self,
        endpoint_id: EndpointId,
    ) -> Result<crate::connection::ConnState> {
        Ok(self
            .peers
            .get(&endpoint_id)
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
