use actor_helper::{Action, Handle, act, act_ok};
use anyhow::Result;
use iroh::{
    EndpointId,
    endpoint::{Connection, VarInt},
    protocol::ProtocolHandler,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, hash_map::Entry},
    time::Duration,
};
use tokio::time::timeout;
use tracing::{debug, error, info, trace};

use crate::{Router, RouterIp, auth, connection::Conn, local_networking::Ipv4Pkg};

#[derive(Debug, Clone)]
pub struct Direct {
    api: Handle<DirectActor, anyhow::Error>,
}

#[derive(Debug)]
struct DirectActor {
    self_handle: Handle<DirectActor, anyhow::Error>,
    network_secret: [u8; 64],
    peers: HashMap<EndpointId, Conn>,
    endpoint: iroh::endpoint::Endpoint,
    rx: actor_helper::Receiver<Action<DirectActor>>,
    direct_connect_tx: tokio::sync::mpsc::Sender<DirectMessage>,
    router: Option<Router>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum DirectMessage {
    IpPacket(Ipv4Pkg),
    IDontLikeWarnings,
}

impl Direct {
    pub const ALPN: &[u8] = b"/iroh/lan-direct/1.0.1";
    pub fn new(
        endpoint: iroh::endpoint::Endpoint,
        direct_connect_tx: tokio::sync::mpsc::Sender<DirectMessage>,
        network_secret: &[u8; 64],
    ) -> Self {
        let (api, rx) = Handle::channel();
        let mut actor = DirectActor {
            self_handle: api.clone(),
            peers: HashMap::new(),
            endpoint,
            rx,
            direct_connect_tx,
            network_secret: *network_secret,
            router: None,
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

    pub async fn ensure_connection(&self, to: EndpointId) -> Result<()> {
        self.api
            .call(act!(actor => actor.ensure_connection(to)))
            .await
    }

    pub async fn set_router(&self, router: Router) -> Result<()> {
        self.api
            .call(act_ok!(actor => async move { actor.set_router(router) }))
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
        let mut cleanup_interval = tokio::time::interval(std::time::Duration::from_secs(10));
        cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("DirectActor run loop started");
        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }
                _ = cleanup_interval.tick() => {
                    self.prune_closed_connections().await;
                }
                _ = tokio::signal::ctrl_c() => {
                     info!("Ctrl-C received, shutting down DirectActor");
                    break
                }
            }
        }
        Ok(())
    }

    async fn prune_closed_connections(&mut self) {
        let mut to_remove = Vec::new();
        for (id, conn) in &self.peers {
            if conn.get_state().await == crate::connection::ConnState::Closed {
                to_remove.push(*id);
            }
        }
        for id in to_remove {
            debug!("Removing closed connection to {}", id);
            self.peers.remove(&id);
        }
    }

    async fn handle_connection(&mut self, conn: iroh::endpoint::Connection) -> Result<()> {
        info!("New direct connection from {:?}", conn.remote_id());
        let remote_id = conn.remote_id();
        if let Some(router) = &self.router {
            if !matches!(router.get_ip_state().await, Ok(RouterIp::AssignedIp(_))) {
                info!(
                    "Accepting connection from {} before local IP assignment",
                    remote_id
                );
            }

            if router.get_ip_from_endpoint_id(remote_id).await.is_err() {
                info!(
                    "Accepting connection from {} before remote IP assignment",
                    remote_id
                );
            }
        } else {
            info!(
                "Accepting connection from {} before router ready",
                remote_id
            );
        }
        let prefer_incoming = self.endpoint.id() > remote_id;

        if let Some(conn_ref) = self.peers.get(&remote_id) {
            let state = conn_ref.get_state().await;
            if state == crate::connection::ConnState::Open && !prefer_incoming {
                info!(
                    "Ignoring incoming connection from {} (keeping existing connection)",
                    remote_id
                );
                conn.close(VarInt::from_u32(409), b"duplicate connection");
                return Ok(());
            }
        }

        let network_secret = self.network_secret;
        let my_id = self.endpoint.id();
        let api = self.self_handle.clone();

        tokio::spawn(async move {
            let accept_result = timeout(
                Duration::from_secs(60),
                auth::accept(&conn, &network_secret, my_id, remote_id),
            )
            .await;

            match accept_result {
                Ok(Ok((send, recv))) => {
                    let _ = api.call(act!(actor => actor.finalize_connection(conn, send, recv))).await;
                }
                Ok(Err(e)) => {
                    error!("Auth accept failed from {}: {}", remote_id, e);
                }
                Err(_) => {
                    error!("Auth accept timed out from {}", remote_id);
                    conn.close(VarInt::from_u32(408), b"accept timeout");
                }
            }
        });

        Ok(())
    }

    async fn finalize_connection(
        &mut self,
        conn: iroh::endpoint::Connection,
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
    ) -> Result<()> {
        let remote_id = conn.remote_id();
        let prefer_incoming = self.endpoint.id() > remote_id;

        match self.peers.entry(remote_id) {
            Entry::Occupied(mut entry) => {
                let state = entry.get().get_state().await;
                if state != crate::connection::ConnState::Open || prefer_incoming {
                    if state != crate::connection::ConnState::Open {
                        info!(
                            "Replacing existing connection to {} in state {:?}",
                            remote_id, state
                        );
                    }
                    debug!(
                        "Existing connection found for {}, upgrading/resetting",
                        remote_id
                    );

                    if let Err(e) = entry
                        .get_mut()
                        .incoming_connection(conn, send, recv)
                        .await
                    {
                        error!("Failed to upgrade connection to {}: {}", remote_id, e);
                    }
                } else {
                    info!(
                        "Ignoring incoming connection from {} (keeping existing connection) - Race detected",
                        remote_id
                    );
                    conn.close(VarInt::from_u32(409), b"duplicate connection");
                }
            }
            Entry::Vacant(entry) => {
                debug!("New connection entry for {}", remote_id);
                match Conn::new(
                    self.endpoint.clone(),
                    conn,
                    send,
                    recv,
                    self.direct_connect_tx.clone(),
                    &self.network_secret,
                )
                .await
                {
                    Ok(new_conn) => {
                        entry.insert(new_conn);
                    }
                    Err(e) => {
                        error!("Failed to create new Conn actor for {}: {}", remote_id, e);
                    }
                }
            }
        }
        Ok(())
    }

    async fn route_packet(&mut self, to: EndpointId, pkg: DirectMessage) -> Result<()> {
        trace!("Routing packet to {}", to);
        match self.peers.entry(to) {
            Entry::Occupied(entry) => {
                if let Err(e) = entry.get().write(pkg).await {
                    error!("Failed to write packet to peer {}: {}", to, e);
                    return Err(e);
                }
            }
            Entry::Vacant(entry) => {
                info!("No active connection to {}, initiating new connection", to);
                if let Some(router) = &self.router {
                    match router.get_ip_state().await {
                        Ok(RouterIp::AssignedIp(_)) => {}
                        _ => {
                            info!("Skipping connect to {}: local IP not assigned", to);
                            return Ok(());
                        }
                    }

                    if router.get_ip_from_endpoint_id(to).await.is_err() {
                        info!("Skipping connect to {}: remote IP not assigned", to);
                        return Ok(());
                    }
                } else {
                    info!("Skipping connect to {}: router not ready", to);
                    return Ok(());
                }
                let conn = Conn::connect(
                    self.endpoint.clone(),
                    to,
                    self.direct_connect_tx.clone(),
                    &self.network_secret,
                )
                .await;

                if let Err(e) = conn.write(pkg).await {
                    error!("Failed to write packet to new connection {}: {}", to, e);
                    return Err(e);
                }
                entry.insert(conn);
            }
        }

        Ok(())
    }

    async fn ensure_connection(&mut self, to: EndpointId) -> Result<()> {
        if self.peers.contains_key(&to) {
            return Ok(());
        }

        if let Some(router) = &self.router {
            match router.get_ip_state().await {
                Ok(RouterIp::AssignedIp(_)) => {}
                _ => {
                    info!(
                        "Skipping ensure_connection to {}: local IP not assigned",
                        to
                    );
                    return Ok(());
                }
            }

            if router.get_ip_from_endpoint_id(to).await.is_err() {
                info!(
                    "Skipping ensure_connection to {}: remote IP not assigned",
                    to
                );
                return Ok(());
            }
        } else {
            info!("Skipping ensure_connection to {}: router not ready", to);
            return Ok(());
        }

        info!("No active connection to {}, initiating new connection", to);
        let conn = Conn::connect(
            self.endpoint.clone(),
            to,
            self.direct_connect_tx.clone(),
            &self.network_secret,
        )
        .await;
        self.peers.insert(to, conn);
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

    fn set_router(&mut self, router: Router) {
        self.router = Some(router);
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
