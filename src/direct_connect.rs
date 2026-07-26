use actor_helper::{Action, Handle, Receiver, act, act_ok};
use anyhow::Result;
use iroh::{
    Endpoint, EndpointId,
    endpoint::{Connection, VarInt},
    protocol::ProtocolHandler,
};
use n0_watcher::Watchable;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    ops::{Add, Div},
    sync::{Arc, atomic::AtomicUsize},
};
use tokio::{
    sync::{Mutex, RwLock},
    time::Instant,
};
use tracing::{debug, error, info, trace, warn};

use crate::{
    connection::{Conn, ConnLiveness, ConnSnapshot, InnerConnState},
    tun::Ipv4Pkg,
};

const MAX_RECONNECT_ATTEMPTS: usize = 100;
const CONN_COUNT_TARGET: usize = 2;

#[derive(Debug, Clone)]
pub struct Direct {
    api: Handle<DirectActor, anyhow::Error>,
}

#[derive(Debug, Clone)]
pub struct ConnGen {
    conn_pool: Arc<Mutex<Vec<Conn>>>,
    conn_counter: Arc<AtomicUsize>,
    last_conn_attempt: Arc<RwLock<Option<Instant>>>,
    attempts: Watchable<usize>,
}

impl Default for ConnGen {
    fn default() -> Self {
        Self {
            conn_pool: Arc::new(Mutex::new(Vec::new())),
            conn_counter: Arc::new(AtomicUsize::new(0)),
            last_conn_attempt: Arc::new(RwLock::new(None)),
            attempts: Watchable::new(0),
        }
    }
}

#[derive(Debug)]
struct DirectActor {
    peers: HashMap<EndpointId, ConnGen>,
    endpoint: iroh::endpoint::Endpoint,
    remote_frame_tx: tokio::sync::mpsc::Sender<(EndpointId, Vec<u8>)>,
    remote_to_tun_tx: tokio::sync::mpsc::Sender<DirectMessage>,
    transmit_frame_rx: tokio::sync::mpsc::Receiver<(EndpointId, Vec<u8>)>,
    latest_frames: HashMap<EndpointId, Vec<u8>>,
    latest_global_frame: Watchable<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum DirectMessage {
    IpPacket(Ipv4Pkg),
    IDontLikeWarnings(Vec<u8>),
}

impl DirectMessage {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::IpPacket(_) => "ip",
            Self::IDontLikeWarnings(_) => "keepalive",
        }
    }

    pub fn payload_len(&self) -> usize {
        match self {
            Self::IpPacket(pkg) => pkg.as_slice().len(),
            Self::IDontLikeWarnings(_) => 0,
        }
    }
}

impl Direct {
    pub const ALPN: &[u8] = b"/iroh/lan-direct/2";
    pub fn new(
        endpoint: iroh::endpoint::Endpoint,
        remote_to_tun_tx: tokio::sync::mpsc::Sender<DirectMessage>,
        transmit_frame_rx: tokio::sync::mpsc::Receiver<(EndpointId, Vec<u8>)>,
        remote_frame_tx: tokio::sync::mpsc::Sender<(EndpointId, Vec<u8>)>,
    ) -> Self {
        let (api, _) = Handle::spawn_with(
            DirectActor {
                peers: HashMap::new(),
                endpoint,
                remote_frame_tx,
                remote_to_tun_tx,
                transmit_frame_rx,
                latest_frames: HashMap::new(),
                latest_global_frame: Watchable::new(Vec::new()),
            },
            |mut actor, rx| async move { actor.run(rx).await },
        );
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
        self.api.call(act!(actor => actor.ensure_peer(to))).await
    }

    pub async fn get_conn_state(&self, endpoint_id: EndpointId) -> Result<InnerConnState> {
        self.api
            .call(act!(actor => actor.get_conn_state(endpoint_id)))
            .await
    }

    pub async fn close(&self) -> Result<()> {
        self.api.call(act!(actor => actor.close())).await
    }

    pub async fn get_connected_peers(&self) -> Result<HashSet<EndpointId>> {
        self.api
            .call(act_ok!(actor => async {
                actor.peers.keys().cloned().collect::<HashSet<EndpointId>>()
            }
            ))
            .await
    }
}

impl DirectActor {
    async fn run(&mut self, rx: Receiver<Action<DirectActor>>) -> Result<(), anyhow::Error> {
        let mut reconnect_interval = tokio::time::interval(std::time::Duration::from_millis(500));
        reconnect_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("DirectActor run loop started");
        loop {
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                    action(self).await;
                }
                _ = reconnect_interval.tick() => {
                    self.connection_driver().await;
                }
                Some((.., bytes)) = self.transmit_frame_rx.recv() => {
                    if bytes.is_empty() {
                        println!("transmit_frame_rx: Received empty frame");
                    }
                    if bytes != self.latest_global_frame.get() {
                        println!("transmit_frame_rx: Setting new frame");
                        let _ = self.latest_global_frame.set(bytes.clone());
                    }
                    /*
                    match self.latest_frames.entry(to) {
                        Entry::Occupied(mut entry) => {
                            if !entry.get().eq(&bytes) {
                                trace!(%to, "updated peer frame");
                                entry.insert(bytes.clone());
                            }
                        }
                        Entry::Vacant(entry) => {
                            debug!(%to, "initial peer frame");
                            entry.insert(bytes.clone());
                        }
                    }
                    if let Some(peer) = self.peers.get_mut(&to) {
                        for conn in peer.conn_pool.lock().await.iter() {
                            conn.set_latest_frame(bytes.clone()).await;
                        }
                    } */
                }
                else => break,
            }
        }
        Ok(())
    }
}

impl DirectActor {
    async fn connection_driver(&mut self) {
        for (id, peer) in self.peers.clone() {
            // check reserve connections for alive
            {
                let mut tbd = Vec::new();
                let mut guard = peer.conn_pool.lock().await;
                for conn in guard.iter_mut() {
                    if !conn.is_alive().await {
                        let snapshot = conn
                            .snapshot()
                            .await
                            .map(|snapshot| snapshot.to_string())
                            .unwrap_or_else(|| {
                                format!("conn_actor_id={} snapshot=unavailable", conn.id())
                            });
                        debug!("iroh-pool-drop-dead peer={} snapshot=[{}]", id, snapshot);
                        conn.drop().await;
                        tbd.push(conn.id());
                        peer.conn_counter
                            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                guard.retain(|conn| !tbd.contains(&conn.id()));

                if peer.conn_counter.load(std::sync::atomic::Ordering::SeqCst) < CONN_COUNT_TARGET
                    && peer.attempts.get() <= MAX_RECONNECT_ATTEMPTS
                    && peer
                        .last_conn_attempt
                        .read()
                        .await
                        .as_ref()
                        .map(|last_attempt| {
                            let should_wait_for = (peer.attempts.get() as f32)
                                .powf(2f32)
                                .div(10f32)
                                .add(2f32)
                                .min(300f32);
                            info!(
                                "last_attempt={} should_wait_for={should_wait_for:?}",
                                last_attempt.elapsed().as_secs_f32()
                            );
                            should_wait_for <= last_attempt.elapsed().as_secs_f32()
                        })
                        .unwrap_or(true)
                {
                    peer.last_conn_attempt.write().await.replace(Instant::now());

                    info!(
                        "Peer {} has not enough open connections and {} attempts, will try to reconnect",
                        id,
                        peer.attempts.get()
                    );
                    drop(guard);
                    self.open_new_connection(
                        id,
                        peer.clone(),
                        self.endpoint.clone(),
                        self.remote_to_tun_tx.clone(),
                    )
                    .await;
                }
            }
        }
    }

    async fn open_new_connection(
        &self,
        remote_eid: EndpointId,
        peer: ConnGen,
        endpoint: Endpoint,
        direct_connect_tx: tokio::sync::mpsc::Sender<DirectMessage>,
    ) {
        peer.conn_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let attempts = peer.attempts.get();
        peer.attempts.set(attempts.saturating_add(1)).ok();

        // Try open new connection
        let last_frame = self.latest_global_frame.clone();//self.latest_frames.get(&remote_eid).cloned().unwrap_or_default();
        let remote_frame_tx = self.remote_frame_tx.clone();
        tokio::spawn(async move {
            info!(%remote_eid, "attempting to open new connection");
            match Conn::open_connection(endpoint.clone(), remote_eid, direct_connect_tx, last_frame, remote_frame_tx).await {
                Ok(new_conn) => {
                    debug!(%remote_eid, "successfully established connection");
                    if new_conn.is_alive().await {
                        let snapshot = new_conn
                            .snapshot()
                            .await
                            .map(|snapshot| snapshot.to_string())
                            .unwrap_or_else(|| {
                                format!("conn_actor_id={} snapshot=unavailable", new_conn.id())
                            });
                        let mut guard = peer.conn_pool.lock().await;
                        guard.push(new_conn.clone());
                        info!(
                            "iroh-pool-add-open peer={} snapshot=[{}]",
                            remote_eid, snapshot
                        );
                        peer.attempts.set(0).ok();
                    } else {
                        debug!(
                            %remote_eid, "new connection is not alive after establishment, dropping"
                        );
                        peer.conn_counter
                            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        new_conn.drop().await;
                    }
                }
                Err(err) => {
                    peer.conn_counter
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    error!("Failed to establish connection to {}: {}", remote_eid, err);
                }
            }
        });
    }

    async fn handle_connection(&mut self, conn: iroh::endpoint::Connection) -> Result<()> {
        let remote_eid = conn.remote_id();
        info!(%remote_eid, "handling new connection");
        let peer = self.peers.entry(remote_eid).or_default().clone();
        let direct_connect_tx = self.remote_to_tun_tx.clone();

        peer.conn_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let remote_eid = conn.remote_id();
        let last_frame = self.latest_global_frame.clone();//self.latest_frames.get(&remote_eid).cloned().unwrap_or_default();
        match Conn::accept_connection(conn.clone(), direct_connect_tx.clone(), last_frame, self.remote_frame_tx.clone()).await {
            Ok(remote_conn) => {
                debug!(%remote_eid, "successfully accepted connection");
                if remote_conn.is_alive().await {
                    let snapshot = remote_conn
                        .snapshot()
                        .await
                        .map(|snapshot| snapshot.to_string())
                        .unwrap_or_else(|| {
                            format!("conn_actor_id={} snapshot=unavailable", remote_conn.id())
                        });
                    let mut guard = peer.conn_pool.lock().await;
                    guard.push(remote_conn.clone());
                    peer.attempts.set(0).ok();
                    info!(
                        "iroh-pool-add-accept peer={} snapshot=[{}]",
                        remote_eid, snapshot
                    );
                } else {
                    debug!(%remote_eid, "accepted connection is not alive after establishment, dropping");
                    remote_conn.drop().await;
                    peer.conn_counter
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    return Err(anyhow::anyhow!(
                        "Accepted connection from {} is not alive after establishment",
                        remote_eid
                    ));
                }
                Ok(())
            }
            Err(err) => {
                error!(%remote_eid, ?err, "failed to accept connection");
                conn.close(VarInt::from_u32(411), b"Failed to accept connection");
                peer.conn_counter
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Err(anyhow::anyhow!(
                    "Failed to accept connection from {}: {}",
                    remote_eid,
                    err
                ))
            }
        }
    }

    async fn route_packet(&mut self, to: EndpointId, pkg: DirectMessage) -> Result<()> {
        //trace!("Routing packet to {}", to);
        match self.peers.entry(to) {
            Entry::Occupied(entry) => {
                let peer = entry.get();
                let msg_kind = pkg.kind_name();
                let payload_len = pkg.payload_len();

                let mut attempt_order: Vec<(u8, u128, Conn, ConnSnapshot)> = Vec::new();
                let mut pool_snapshots = Vec::new();
                let guard = peer.conn_pool.lock().await;
                for conn in guard.iter() {
                    if let Some(snapshot) = conn.snapshot().await {
                        pool_snapshots.push(snapshot.to_string());
                        if snapshot.liveness != ConnLiveness::Dead {
                            let priority = match snapshot.liveness {
                                ConnLiveness::Usable => 0,
                                ConnLiveness::Suspect => 1,
                                ConnLiveness::Dead => 2,
                            };
                            attempt_order.push((
                                priority,
                                snapshot.idle_for_ms,
                                conn.clone(),
                                snapshot,
                            ));
                        }
                    }
                }
                drop(guard);
                attempt_order
                    .sort_by_key(|(priority, idle_for_ms, _, _)| (*priority, *idle_for_ms));

                /*
                debug!(
                    "iroh-route-pool peer={} kind={} payload_bytes={} pool_size={} open_candidates={} snapshots=[{}]",
                    to,
                    msg_kind,
                    payload_len,
                    pool_snapshots.len(),
                    attempt_order.len(),
                    if pool_snapshots.is_empty() {
                        "none".to_string()
                    } else {
                        pool_snapshots.join(" || ")
                    }
                );
                */

                for (_, _idle_for_ms, conn, snapshot) in attempt_order {
                    /*debug!(
                        "iroh-route-selected peer={} kind={} payload_bytes={} attempted_liveness={} attempted_idle_for_ms={} snapshot=[{}]",
                        to, msg_kind, payload_len, snapshot.liveness, idle_for_ms, snapshot
                    );*/
                    if conn.write(pkg.clone()).await.is_ok() {
                        return Ok(());
                    }
                    warn!(
                        "iroh-route-write-failed peer={} kind={} payload_bytes={} snapshot=[{}]",
                        to, msg_kind, payload_len, snapshot
                    );
                }
                error!("Failed to write packet to peer {}: no open connections", to);
                Err(anyhow::anyhow!(
                    "Failed to write packet to peer {}: no open connections",
                    to
                ))
            }
            Entry::Vacant(entry) => {
                info!("No active connection to {}, initiating new connection", to);
                entry.insert(Default::default());
                Err(anyhow::anyhow!(
                    "No active connection to {}, initiating new connection",
                    to
                ))
            }
        }
    }

    async fn ensure_peer(&mut self, to: EndpointId) -> Result<()> {
        if self.peers.contains_key(&to) {
            return Ok(());
        }

        info!(
            "No active connection to {}, initiating new connection (ensure_peer)",
            to
        );
        self.peers.insert(to, Default::default());
        Ok(())
    }

    pub async fn get_conn_state(&self, endpoint_id: EndpointId) -> Result<InnerConnState> {
        let peer = self
            .peers
            .get(&endpoint_id)
            .ok_or(anyhow::anyhow!("no connection to peer"))?;

        let guard = peer.conn_pool.lock().await;
        for conn in guard.iter() {
            if conn.is_alive().await {
                return Ok(InnerConnState::Open);
            }
        }
        Ok(InnerConnState::Closed)
    }

    pub async fn close(&mut self) -> Result<()> {
        for (_, conn_gen) in self.peers.drain() {
            let guard = conn_gen.conn_pool.lock().await;
            conn_gen
                .conn_counter
                .store(0, std::sync::atomic::Ordering::SeqCst);
            for conn in guard.iter() {
                conn.drop().await;
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
        info!("ProtocolHandler: new conn: {}", connection.remote_id());
        let _ = self.handle_connection(connection).await;
        Ok(())
    }
}
