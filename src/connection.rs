use std::fmt::{self, Display};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Duration};

use crate::DirectMessage;
use actor_helper::{Action, ActorState, Handle, Receiver, act, act_ok};
use anyhow::Result;
use bytes::Bytes;
use futures_lite::StreamExt;
use iroh::address_lookup::{AddressLookup, DnsAddressLookup};
use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use n0_watcher::Watchable;
use tokio::sync::Mutex;
use tokio::time::{self, Instant};
use tracing::{debug, info, trace, warn};

const QUEUE_SIZE: usize = 1024 * 16;
const BACKPRESSURE_WARN_MS: u128 = 5;
const MAX_SENDER_QUEUE: usize = 50_000;
const WRITE_CHANNEL_CAP: usize = 8_192;
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const QUEUE_WARN_LEN: usize = 10_000;
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(2000);
const CONNECTING_TIMEOUT: Duration = Duration::from_secs(15);
const DATAGRAM_PREFIX: u8 = 0x43;
const MAX_CONSECUTIVE_READ_TIMEOUTS: usize = 3;
const MAX_CONSECUTIVE_WRITE_TIMEOUTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InnerConnState {
    Connecting,
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConnLiveness {
    Usable,
    Suspect,
    Dead,
}

impl Display for ConnLiveness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usable => write!(f, "usable"),
            Self::Suspect => write!(f, "suspect"),
            Self::Dead => write!(f, "dead"),
        }
    }
}

#[derive(Debug, Default)]
struct ConnHealthWindow {
    last_rx_count: usize,
    last_tx_count: usize,
    last_udp_rx: u64,
    last_udp_tx: u64,
    last_lost: u64,
    consecutive_read_timeouts: usize,
    consecutive_write_timeouts: usize,
    no_active_paths: bool,
}

#[derive(Debug, Clone)]
pub struct ConnPathSnapshot {
    pub path_id: String,
    pub remote_addr: String,
    pub is_selected: bool,
    pub is_closed: bool,
    pub rtt_ms: Option<u128>,
    pub lost_packets: Option<u64>,
    pub black_holes_detected: Option<u64>,
    pub current_mtu: Option<u16>,
    pub udp_tx_datagrams: Option<u64>,
    pub udp_rx_datagrams: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ConnSnapshot {
    pub conn_actor_id: u64,
    pub peer: EndpointId,
    pub quic_stable_id: Option<usize>,
    pub side: &'static str,
    pub state: InnerConnState,
    pub liveness: ConnLiveness,
    pub idle_for_ms: u128,
    pub rx_count: usize,
    pub tx_count: usize,
    pub queue_len: usize,
    pub write_timeouts: usize,
    pub consecutive_write_errors: usize,
    pub consecutive_read_timeouts: usize,
    pub consecutive_write_timeouts: usize,
    pub no_active_paths: bool,
    pub dropped_packets: usize,
    pub total_lost_packets: Option<u64>,
    pub total_udp_tx_datagrams: Option<u64>,
    pub total_udp_rx_datagrams: Option<u64>,
    pub selected_path: Option<ConnPathSnapshot>,
    pub paths: Vec<ConnPathSnapshot>,
}

impl fmt::Display for ConnPathSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "path_id={} remote_addr={} selected={} closed={} rtt_ms={} lost_packets={} black_holes={} mtu={} udp_tx_dgrams={} udp_rx_dgrams={}",
            self.path_id,
            self.remote_addr,
            self.is_selected,
            self.is_closed,
            self.rtt_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.lost_packets
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.black_holes_detected
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.current_mtu
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.udp_tx_datagrams
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.udp_rx_datagrams
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )
    }
}

impl fmt::Display for ConnSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let selected_path = self
            .selected_path
            .as_ref()
            .map(|path| path.to_string())
            .unwrap_or_else(|| "none".to_string());
        let all_paths = if self.paths.is_empty() {
            "none".to_string()
        } else {
            self.paths
                .iter()
                .map(|path| path.to_string())
                .collect::<Vec<_>>()
                .join(" | ")
        };

        write!(
            f,
            "conn_actor_id={} peer={} quic_stable_id={} side={} state={:?} liveness={} idle_for_ms={} rx_count={} tx_count={} queue_len={} write_timeouts={} consecutive_write_errors={} consecutive_read_timeouts={} consecutive_write_timeouts={} no_active_paths={} dropped_packets={} total_lost_packets={} total_udp_tx_dgrams={} total_udp_rx_dgrams={} selected_path=[{}] paths=[{}]",
            self.conn_actor_id,
            self.peer,
            self.quic_stable_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.side,
            self.state,
            self.liveness,
            self.idle_for_ms,
            self.rx_count,
            self.tx_count,
            self.queue_len,
            self.write_timeouts,
            self.consecutive_write_errors,
            self.consecutive_read_timeouts,
            self.consecutive_write_timeouts,
            self.no_active_paths,
            self.dropped_packets,
            self.total_lost_packets
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.total_udp_tx_datagrams
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.total_udp_rx_datagrams
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            selected_path,
            all_paths
        )
    }
}

fn side_label(is_open_side: bool) -> &'static str {
    if is_open_side { "open" } else { "accept" }
}

fn snapshot_paths(conn: &Connection) -> Vec<ConnPathSnapshot> {
    conn.paths()
        .into_iter()
        .map(|path| {
            let stats = path.stats();
            ConnPathSnapshot {
                path_id: format!("{:?}", path.id()),
                remote_addr: format!("{:?}", path.remote_addr()),
                is_selected: path.is_selected(),
                is_closed: false,
                rtt_ms: Some(path.rtt().as_millis()),
                lost_packets: Some(stats.lost_packets),
                black_holes_detected: Some(stats.black_holes_detected),
                current_mtu: Some(stats.current_mtu),
                udp_tx_datagrams: Some(stats.udp_tx.datagrams),
                udp_rx_datagrams: Some(stats.udp_rx.datagrams),
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Conn {
    api: Handle<ConnActor, anyhow::Error>,
    id: u64,
}

impl Eq for Conn {}

impl PartialEq for Conn {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Debug)]
struct ConnActor {
    self_handle: Option<Handle<ConnActor, anyhow::Error>>,

    // all of these need to be optionals so that we can create an empty
    // shell of the actor and then fill in the values later so we don't wait
    // forever in the main standalone loop for router events hanging on
    // route_packet failed
    conn: Option<Connection>,
    conn_endpoint_id: EndpointId,
    id: u64,


    external_sender: tokio::sync::mpsc::Sender<DirectMessage>,

    write_task: Option<tokio::task::JoinHandle<()>>,
    write_tx: Option<tokio::sync::mpsc::Sender<DirectMessage>>,

    read_task: Option<tokio::task::JoinHandle<()>>,

    connected_task: Option<tokio::task::JoinHandle<()>>,

    queue_len: Arc<std::sync::atomic::AtomicUsize>,
    dropped_packets: Arc<AtomicUsize>,

    sender_queue: VecDeque<DirectMessage>,
    rx_count: Arc<AtomicUsize>,
    tx_count: Arc<AtomicUsize>,
    write_timeouts: Arc<AtomicUsize>,
    consecutive_write_errors: Arc<AtomicUsize>,

    is_open_side: bool,
    conn_state: Arc<Mutex<InnerConnState>>,
    last_keep_alive: Arc<Mutex<Instant>>,
    health: Arc<Mutex<ConnHealthWindow>>,

    dest_ip: Option<Ipv4Addr>,

    latest_frame: Watchable<Vec<u8>>,
    remote_frame_tx: tokio::sync::mpsc::Sender<(EndpointId, Vec<u8>)>,
}

impl Conn {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub async fn is_alive(&self) -> bool {
        matches!(
            self.snapshot().await.map(|snapshot| snapshot.liveness),
            Some(ConnLiveness::Usable | ConnLiveness::Suspect)
        )
    }

    pub async fn snapshot(&self) -> Option<ConnSnapshot> {
        self.api
            .call(act_ok!(actor => async move { actor.snapshot().await }))
            .await
            .ok()
    }

    pub async fn accept_connection(
        conn: iroh::endpoint::Connection,
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
        latest_frame: Watchable<Vec<u8>>,
        remote_frame_tx: tokio::sync::mpsc::Sender<(EndpointId, Vec<u8>)>
    ) -> Result<Self> {
        let conn_state = Arc::new(Mutex::new(InnerConnState::Connecting));
        let last_keep_alive = Arc::new(Mutex::new(Instant::now()));
        let id = rand::random();
        let (api, _) = Handle::spawn_with(
            ConnActor::new(
                id,
                external_sender,
                conn.remote_id(),
                false,
                conn_state.clone(),
                last_keep_alive.clone(),
                latest_frame,
                remote_frame_tx
            ),
            |mut actor, rx| async move { actor.run(rx).await },
        );
        let s = Self { id, api };
        let self_handle = s.api.clone();
        s.api
            .call(act_ok!(actor => async move {
                actor.self_handle = Some(self_handle);
            }))
            .await?;

        if let Err(err) = s.establish_connection(conn.clone()).await {
            warn!(
                "Failed to establish connection with {}: {}",
                conn.remote_id(),
                err
            );
            s.drop().await;
            anyhow::bail!("Failed to establish connection: {}", err);
        }

        Ok(s)
    }

    pub async fn open_connection(
        endpoint: Endpoint,
        remote_endpoint_id: EndpointId,
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
        latest_frame: Watchable<Vec<u8>>,
        remote_frame_tx: tokio::sync::mpsc::Sender<(EndpointId, Vec<u8>)>,
    ) -> Result<Self> {
        let conn_state = Arc::new(Mutex::new(InnerConnState::Connecting));
        let last_keep_alive = Arc::new(Mutex::new(Instant::now()));
        let id = rand::random();
        let (api, _) = Handle::spawn_with(
            ConnActor::new(
                id,
                external_sender,
                remote_endpoint_id,
                true,
                conn_state.clone(),
                last_keep_alive.clone(),
                latest_frame,
                remote_frame_tx,
            ),
            |mut actor, rx| async move { actor.run(rx).await },
        );
        let s = Self { id, api };
        let self_handle = s.api.clone();
        s.api
            .call(act_ok!(actor => async move {
                actor.self_handle = Some(self_handle);
            }))
            .await?;

        let endpoint_addr = resolve_addr(&endpoint, remote_endpoint_id).await;
        info!("Connecting to EndpointAddr: {:?}", endpoint_addr);

        let conn = match tokio::time::timeout(
            CONNECTING_TIMEOUT,
            endpoint.connect(endpoint_addr, crate::Direct::ALPN),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                warn!(
                    "Initial connection to {} failed: {:?}",
                    remote_endpoint_id, e
                );
                s.drop().await;
                anyhow::bail!("Failed to establish connection: {}: {:?}", e, e);
            }
            Err(_) => {
                warn!(
                    "Initial connection to {} timed out after {}s",
                    remote_endpoint_id,
                    CONNECTING_TIMEOUT.as_secs()
                );
                s.drop().await;
                anyhow::bail!("Connection timed out");
            }
        };

        if let Err(err) = s.establish_connection(conn).await {
            warn!(
                "Failed to establish connection with {}: {}",
                remote_endpoint_id, err
            );
            s.drop().await;
            anyhow::bail!("Failed to establish connection: {}: {:?}", err, err);
        }

        Ok(s)
    }

    pub async fn write(&self, pkg: DirectMessage) -> Result<()> {
        self.api.call(act_ok!(actor => actor.write(pkg))).await
    }

    pub async fn establish_connection(&self, conn: Connection) -> Result<()> {
        self.api
            .call(act!(actor => actor.establish_connection(conn)))
            .await
    }

    pub async fn drop(&self) {
        if self.api.state() == ActorState::Stopped {
            return;
        }
        self.api
            .call(act_ok!(actor => async {
                actor.close().await;
            }))
            .await
            .ok();
    }
}

#[allow(dead_code)]
async fn handshake(conn: Connection, handshake_timeout: Duration) -> Result<()> {
    let handshake_task = async {
        let (mut send, mut recv) = if conn.side() == iroh::endpoint::Side::Client {
            conn.open_bi().await?
        } else {
            conn.accept_bi().await?
        };
        let mut buf = [0u8; 1];
        info!(
            "Performing connection handshake for #1: {}",
            conn.remote_id()
        );
        send.write_all(&buf).await?;
        send.finish()?;

        info!(
            "Performing connection handshake for #2: {}",
            conn.remote_id()
        );
        recv.read_exact(&mut buf).await?;
        recv.read_to_end(usize::MAX).await?;
        info!(
            "Performing connection handshake for #3: {}",
            conn.remote_id()
        );
        Ok(())
    };

    tokio::time::timeout(handshake_timeout, handshake_task).await?
}

impl ConnActor {
    async fn run(&mut self, rx: Receiver<Action<ConnActor>>) -> Result<()> {
        //let mut reconnect_ticker = tokio::time::interval(Duration::from_millis(500));
        //reconnect_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut keepalive_ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut stats_ticker = tokio::time::interval(STATS_LOG_INTERVAL);
        stats_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("ConnActor started for peer: {}", self.conn_endpoint_id);

        loop {
            let current_state = {
                let state = self.conn_state.lock().await;
                state.clone()
            };
            if current_state == InnerConnState::Closed {
                debug!(
                    "ConnActor for {} is in Closed state, exiting run loop",
                    self.conn_endpoint_id
                );
                break;
            }
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                    action(self).await;
                }
                _ = keepalive_ticker.tick() => {
                    if current_state != InnerConnState::Open {
                        continue;
                    }
                    if let Some(tx) = &self.write_tx {
                        let latest_frame = self.latest_frame.get();
                        if latest_frame.is_empty() {
                            println!("Sending empty KeepAlive");
                        }
                        match tx.try_send(DirectMessage::IDontLikeWarnings(latest_frame.clone())) {
                            Ok(_) => {
                                //info!("Sent keepalive, i am {}", if self.is_open_side { "OPEN" } else { "ACCEPT" });
                                let new_len = self.queue_len.fetch_add(1, Ordering::Relaxed) + 1;
                                if new_len > QUEUE_WARN_LEN {
                                    warn!("Stream queue length high (keepalive): {}", new_len);
                                }
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                _ = stats_ticker.tick() => {
                    let q_len = self.queue_len.load(Ordering::Relaxed);
                    if q_len > 100 {
                        warn!("[PROBE-QUEUE] High Queue Len: {}", q_len);
                    }
                    debug!(
                        "{}-{} Conn stats: endpoint_id={} ip={:?} state={:?} rx_count={} tx_count={} queue_len={} write_timeouts={} write_errors={} dropped_packets={}",
                        if self.is_open_side { "[OPEN]" } else { "[ACCEPT]" },
                        self.id,
                        self.conn_endpoint_id,
                        self.dest_ip,
                        current_state,
                        self.rx_count.load(Ordering::Relaxed),
                        self.tx_count.load(Ordering::Relaxed),
                        self.queue_len.load(Ordering::Relaxed),
                        self.write_timeouts.load(Ordering::Relaxed),
                        self.consecutive_write_errors.load(Ordering::Relaxed),
                        self.dropped_packets.load(Ordering::Relaxed)
                    );
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl-C, stopping actor");
                    break
                }
            }
        }
        Ok(())
    }
}

impl ConnActor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
        conn_endpoint_id: EndpointId,
        is_open_side: bool,
        conn_state: Arc<Mutex<InnerConnState>>,
        last_keep_alive: Arc<Mutex<Instant>>,
        latest_frame: Watchable<Vec<u8>>,
        remote_frame_tx: tokio::sync::mpsc::Sender<(EndpointId, Vec<u8>)>,
    ) -> Self {
        if latest_frame.get().is_empty() {
            println!("New: Setting empty frame");
        }
        Self {
            id,
            external_sender,
            read_task: None,
            write_task: None,
            write_tx: None,
            connected_task: None,
            queue_len: Arc::new(AtomicUsize::new(0)),
            sender_queue: VecDeque::with_capacity(QUEUE_SIZE),
            conn: None,
            conn_endpoint_id,
            self_handle: None,
            rx_count: Arc::new(AtomicUsize::new(0)),
            tx_count: Arc::new(AtomicUsize::new(0)),
            write_timeouts: Arc::new(AtomicUsize::new(0)),
            consecutive_write_errors: Arc::new(AtomicUsize::new(0)),
            dropped_packets: Arc::new(AtomicUsize::new(0)),
            is_open_side,
            conn_state,
            last_keep_alive,
            health: Arc::new(Mutex::new(ConnHealthWindow::default())),
            dest_ip: None,
            latest_frame,
            remote_frame_tx,
        }
    }

    pub async fn close(&mut self) {
        {
            let mut state_guard = self.conn_state.lock().await;
            if *state_guard == InnerConnState::Closed {
                debug!(
                    "ConnActor for {} already in Closed state, skipping close",
                    self.conn_endpoint_id
                );
                return;
            }
            *state_guard = InnerConnState::Closed;
        }

        info!("Closing connection actor");
        if let Some(conn) = self.conn.as_mut() {
            conn.close(VarInt::from_u32(400), b"Connection closed by user");
        }
        self.conn = None;

        if let Some(task) = self.read_task.take() {
            task.abort();
        }
        if let Some(task) = self.write_task.take() {
            task.abort();
        }
        if let Some(task) = self.connected_task.take() {
            task.abort();
        }
    }

    pub async fn write(&mut self, pkg: DirectMessage) {
        if self.dest_ip.is_none()
            && let DirectMessage::IpPacket(pkg) = &pkg
            && let Ok(ip) = pkg.to_ipv4_packet()
        {
            self.dest_ip = Some(ip.get_destination());
        }

        if let Some(tx) = &self.write_tx {
            trace!("Sending packet to write task");
            match tx.try_send(pkg) {
                Ok(_) => {
                    let new_len = self.queue_len.fetch_add(1, Ordering::Relaxed) + 1;
                    if new_len > QUEUE_WARN_LEN {
                        warn!("Stream queue length high: {}", new_len);
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                    if self
                        .dropped_packets
                        .load(Ordering::Relaxed)
                        .is_multiple_of(1000)
                    {
                        warn!(
                            "Write queue full, dropping packet (dropped={})",
                            self.dropped_packets.load(Ordering::Relaxed)
                        );
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(val)) => {
                    warn!("Write task channel closed, buffering packet.");
                    self.sender_queue.push_front(val);
                    while self.sender_queue.len() > MAX_SENDER_QUEUE {
                        self.sender_queue.pop_back();
                    }
                    self.close().await;
                }
            }
        } else {
            trace!(
                "Queueing packet for write. Queue size: {}",
                self.sender_queue.len()
            );
            self.sender_queue.push_front(pkg);
            while self.sender_queue.len() > MAX_SENDER_QUEUE {
                self.sender_queue.pop_back();
            }
        }
    }

    pub async fn establish_connection(&mut self, conn: Connection) -> Result<()> {
        info!("Incoming connection from: {}", conn.remote_id());
        let self_handle = if let Some(api) = self.self_handle.clone() {
            api
        } else {
            warn!("No API handle provided to read loop, cannot close connection on failure");
            return Err(anyhow::anyhow!("internal error: no API handle"));
        };

        if conn.close_reason().is_some() {
            warn!("Incoming connection already closed");
            self.close().await;
            return Err(anyhow::anyhow!("connection closed"));
        }

        info!("Spawning read task for incoming connection");
        let rx_count = self.rx_count.clone();
        self.read_task = Some(tokio::spawn(retry_read_loop(
            conn.clone(),
            self.external_sender.clone(),
            self_handle.clone(),
            self.id,
            rx_count,
            self.last_keep_alive.clone(),
            self.health.clone(),
            self.remote_frame_tx.clone(),
        )));

        info!("Spawning write task for incoming connection");
        let (tx, rx) = tokio::sync::mpsc::channel(WRITE_CHANNEL_CAP);
        self.queue_len.store(0, Ordering::Relaxed);
        let write_timeouts = self.write_timeouts.clone();
        let tx_count = self.tx_count.clone();
        self.write_task = Some(tokio::spawn(write_loop_bounded(
            conn.clone(),
            rx,
            self_handle.clone(),
            self.id,
            self.queue_len.clone(),
            "main",
            tx_count,
            write_timeouts,
            self.health.clone(),
        )));
        self.write_tx = Some(tx.clone());

        self.connected_task = Some(tokio::spawn(connection_watcher_loop(
            conn.clone(),
            self_handle.clone(),
            self.id,
            side_label(self.is_open_side),
            self.health.clone(),
        )));

        self.conn = Some(conn);
        if self.conn.is_some() {
            let snapshot = self.snapshot().await;
            info!("iroh-conn-established {}", snapshot);
            debug!(
                "iroh-qlog-filename-hint peer={} conn_actor_id={} quic_stable_id={} side={} qlog_filename_format=<prefix><unix_ms>-<initial_dst_cid>-<{}>.qlog",
                self.conn_endpoint_id,
                self.id,
                snapshot
                    .quic_stable_id
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                snapshot.side,
                snapshot.side
            );
        }
        self.consecutive_write_errors.store(0, Ordering::Relaxed);
        self.rx_count.store(0, Ordering::Relaxed);
        self.tx_count.store(0, Ordering::Relaxed);
        {
            let mut health = self.health.lock().await;
            *health = ConnHealthWindow::default();
        }

        while let Some(msg) = self.sender_queue.pop_back() {
            match tx.try_send(msg) {
                Ok(_) => {
                    let new_len = self.queue_len.fetch_add(1, Ordering::Relaxed) + 1;
                    if new_len > QUEUE_WARN_LEN {
                        warn!("Stream queue length high (flush): {}", new_len);
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "Write task channel full while flushing queued messages, dropping {} messages",
                        self.sender_queue.len()
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "Write task channel closed while flushing queued messages, dropping {} messages",
                        self.sender_queue.len()
                    );
                    self.close().await;
                    return Err(anyhow::anyhow!("connection closed"));
                }
            }
        }

        let mut state_guard = self.conn_state.lock().await;
        *state_guard = InnerConnState::Open;

        Ok(())
    }

    pub async fn snapshot(&self) -> ConnSnapshot {
        let state = self.conn_state.lock().await.clone();
        let keep_alive = *self.last_keep_alive.lock().await;
        let idle_for_ms = Instant::now().duration_since(keep_alive).as_millis();
        let (
            quic_stable_id,
            selected_path,
            paths,
            total_lost_packets,
            total_udp_tx_datagrams,
            total_udp_rx_datagrams,
        ) = if let Some(conn) = self.conn.as_ref() {
            let paths = snapshot_paths(conn);
            let selected_path = paths.iter().find(|path| path.is_selected).cloned();
            let stats = conn.stats();
            (
                Some(conn.stable_id()),
                selected_path,
                paths,
                Some(stats.lost_packets),
                Some(stats.udp_tx.datagrams),
                Some(stats.udp_rx.datagrams),
            )
        } else {
            (None, None, Vec::new(), None, None, None)
        };

        let rx_count = self.rx_count.load(Ordering::Relaxed);
        let tx_count = self.tx_count.load(Ordering::Relaxed);
        let write_timeouts = self.write_timeouts.load(Ordering::Relaxed);
        let consecutive_write_errors = self.consecutive_write_errors.load(Ordering::Relaxed);
        let dropped_packets = self.dropped_packets.load(Ordering::Relaxed);

        let total_lost = total_lost_packets.unwrap_or(0);
        let total_udp_tx = total_udp_tx_datagrams.unwrap_or(0);
        let total_udp_rx = total_udp_rx_datagrams.unwrap_or(0);

        let mut health = self.health.lock().await;
        let rx_progress = rx_count > health.last_rx_count || total_udp_rx > health.last_udp_rx;
        let tx_progress = tx_count > health.last_tx_count || total_udp_tx > health.last_udp_tx;
        let loss_jump = total_lost > health.last_lost;

        let liveness = classify_liveness(
            state.clone(),
            selected_path.as_ref(),
            &paths,
            rx_progress,
            tx_progress,
            loss_jump,
            &health,
        );

        let snapshot = ConnSnapshot {
            conn_actor_id: self.id,
            peer: self.conn_endpoint_id,
            quic_stable_id,
            side: side_label(self.is_open_side),
            state,
            liveness,
            idle_for_ms,
            rx_count,
            tx_count,
            queue_len: self.queue_len.load(Ordering::Relaxed),
            write_timeouts,
            consecutive_write_errors,
            consecutive_read_timeouts: health.consecutive_read_timeouts,
            consecutive_write_timeouts: health.consecutive_write_timeouts,
            no_active_paths: health.no_active_paths,
            dropped_packets,
            total_lost_packets,
            total_udp_tx_datagrams,
            total_udp_rx_datagrams,
            selected_path,
            paths,
        };

        health.last_rx_count = rx_count;
        health.last_tx_count = tx_count;
        health.last_udp_rx = total_udp_rx;
        health.last_udp_tx = total_udp_tx;
        health.last_lost = total_lost;

        snapshot
    }
}

fn classify_liveness(
    state: InnerConnState,
    selected_path: Option<&ConnPathSnapshot>,
    paths: &[ConnPathSnapshot],
    rx_progress: bool,
    tx_progress: bool,
    loss_jump: bool,
    health: &ConnHealthWindow,
) -> ConnLiveness {
    if state != InnerConnState::Open {
        return ConnLiveness::Dead;
    }

    if health.no_active_paths {
        return ConnLiveness::Dead;
    }

    let has_open_path = paths.iter().any(|path| !path.is_closed);
    if !has_open_path {
        return ConnLiveness::Dead;
    }

    let has_selected_open_path = selected_path.is_some_and(|path| !path.is_closed);
    let blackhole_seen = selected_path
        .and_then(|path| path.black_holes_detected)
        .unwrap_or(0)
        > 0;

    if has_selected_open_path && rx_progress {
        return ConnLiveness::Usable;
    }

    if has_selected_open_path
        && tx_progress
        && health.consecutive_read_timeouts < MAX_CONSECUTIVE_READ_TIMEOUTS
        && health.consecutive_write_timeouts < MAX_CONSECUTIVE_WRITE_TIMEOUTS
        && !blackhole_seen
    {
        return ConnLiveness::Suspect;
    }

    if !has_selected_open_path
        && has_open_path
        && health.consecutive_read_timeouts < MAX_CONSECUTIVE_READ_TIMEOUTS
    {
        return ConnLiveness::Suspect;
    }

    if health.consecutive_read_timeouts >= MAX_CONSECUTIVE_READ_TIMEOUTS
        && !rx_progress
        && (blackhole_seen
            || loss_jump
            || health.consecutive_write_timeouts >= MAX_CONSECUTIVE_WRITE_TIMEOUTS)
    {
        return ConnLiveness::Dead;
    }

    ConnLiveness::Suspect
}

async fn connection_watcher_loop(
    conn: Connection,
    api: Handle<ConnActor, anyhow::Error>,
    conn_actor_id: u64,
    side: &'static str,
    health: Arc<Mutex<ConnHealthWindow>>,
) {
    let mut watcher = conn.paths_stream();
    while let Some(update) = watcher.next().await {
        let path_snapshot = snapshot_paths(&conn)
            .into_iter()
            .map(|path| path.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        debug!(
            "iroh-path-update peer={} conn_actor_id={} quic_stable_id={} side={} update={:?} paths=[{}]",
            conn.remote_id(),
            conn_actor_id,
            conn.stable_id(),
            side,
            update,
            path_snapshot
        );
        if conn.weak_handle().upgrade().is_none() {
            warn!(
                "iroh-path-abandoned peer={} conn_actor_id={} quic_stable_id={} side={} paths=[{}]",
                conn.remote_id(),
                conn_actor_id,
                conn.stable_id(),
                side,
                path_snapshot
            );
            break;
        }
    }

    // all paths abandoned, connection is dead
    warn!(
        "iroh-no-active-paths peer={} conn_actor_id={} quic_stable_id={} side={}",
        conn.remote_id(),
        conn_actor_id,
        conn.stable_id(),
        side
    );
    {
        let mut health = health.lock().await;
        health.no_active_paths = true;
    }
    let _ = api.call(act_ok!(actor => actor.close())).await;
}

#[allow(clippy::too_many_arguments)]
async fn write_loop_bounded(
    conn: Connection,
    mut rx: tokio::sync::mpsc::Receiver<DirectMessage>,
    api: Handle<ConnActor, anyhow::Error>,
    conn_actor_id: u64,
    queue_len: Arc<std::sync::atomic::AtomicUsize>,
    label: &'static str,
    tx_count: Arc<AtomicUsize>,
    write_timeout: Arc<AtomicUsize>,
    health: Arc<Mutex<ConnHealthWindow>>,
) {
    info!("Write task started ({})", label);
    while let Some(msg) = rx.recv().await {
        let _ = queue_len.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_sub(1))
        });
        let msg_kind = msg.kind_name();
        let payload_len = msg.payload_len();
        let bytes = match postcard::to_stdvec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to serialize message: {}", e);
                continue;
            }
        };
        let mut buf = vec![DATAGRAM_PREFIX];
        buf.extend_from_slice(&bytes);
        let datagram_len = buf.len();

        let mut retries = 0;
        while retries < 1 {
            /*debug!(
                "iroh-send peer={} conn_actor_id={} quic_stable_id={} kind={} payload_bytes={} datagram_bytes={}",
                conn.remote_id(),
                conn_actor_id,
                conn.stable_id(),
                msg_kind,
                payload_len,
                datagram_len
            );*/
            match time::timeout(KEEPALIVE_INTERVAL * 3, async {
                conn.send_datagram(Bytes::from(buf.clone()))
            })
            .await
            {
                Ok(Ok(())) => {
                    {
                        let mut health = health.lock().await;
                        health.consecutive_write_timeouts = 0;
                    }
                    /*debug!(
                        "iroh-send-ok peer={} conn_actor_id={} quic_stable_id={} kind={} payload_bytes={} datagram_bytes={}",
                        conn.remote_id(),
                        conn_actor_id,
                        conn.stable_id(),
                        msg_kind,
                        payload_len,
                        datagram_len
                    );*/
                    break;
                }
                Ok(Err(err)) => {
                    warn!("Write error (frame): {}, {:?}, retrying...", err, err);
                    {
                        let mut health = health.lock().await;
                        health.consecutive_write_timeouts =
                            health.consecutive_write_timeouts.saturating_add(1);
                    }
                    retries += 1;
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => {
                    warn!("Write error timeout (frame)");
                    write_timeout.fetch_add(1, Ordering::SeqCst);
                    {
                        let mut health = health.lock().await;
                        health.consecutive_write_timeouts =
                            health.consecutive_write_timeouts.saturating_add(1);
                    }
                    time::sleep(Duration::from_millis(100)).await;
                    break;
                }
            }
        }

        if retries >= 1 {
            let liveness = api
                .call(act_ok!(actor => async move { actor.snapshot().await.liveness }))
                .await
                .unwrap_or(ConnLiveness::Dead);
            warn!(
                "Write failed after 1 retries, keeping connection in {:?} state for peer_id: {}",
                liveness,
                if let Ok(peer_id) = api
                    .call(act_ok!(actor => async move { actor.conn_endpoint_id }))
                    .await
                {
                    peer_id.to_string()
                } else {
                    "unknown".to_string()
                }
            );
            if liveness == ConnLiveness::Dead {
                info!("Write task stopped ({})", label);
                let _ = api.call(act_ok!(actor => actor.close())).await;
                break;
            }
            continue;
        }

        tx_count.fetch_add(1, Ordering::SeqCst);
    }
}

async fn retry_read_loop(
    conn: Connection,
    sender: tokio::sync::mpsc::Sender<DirectMessage>,
    api: Handle<ConnActor, anyhow::Error>,
    conn_actor_id: u64,
    rx_count: Arc<AtomicUsize>,
    last_keep_alive: Arc<Mutex<Instant>>,
    health: Arc<Mutex<ConnHealthWindow>>,
    remote_frame_tx: tokio::sync::mpsc::Sender<(EndpointId, Vec<u8>)>,
) {
    info!("Read task started");
    loop {
        match tokio::time::timeout(KEEPALIVE_INTERVAL * 3, read_next_msg(&conn)).await {
            Ok(Ok((msg, datagram_len))) => {
                {
                    let mut health = health.lock().await;
                    health.consecutive_read_timeouts = 0;
                }
                rx_count.fetch_add(1, Ordering::SeqCst);
                //trace!("Read message from stream, forwarding to network actor");
                let start = std::time::Instant::now();
                let msg_kind = msg.kind_name();
                let payload_len = msg.payload_len();
                /*debug!(
                    "iroh-raw-recv peer={} conn_actor_id={} quic_stable_id={} kind={} payload_bytes={} datagram_bytes={}",
                    conn.remote_id(),
                    conn_actor_id,
                    conn.stable_id(),
                    msg_kind,
                    payload_len,
                    datagram_len
                );*/
                let mut last_keep_alive = last_keep_alive.lock().await;
                *last_keep_alive = Instant::now();
                if let DirectMessage::IDontLikeWarnings(frame) = msg {
                    /*debug!(
                        "Received keepalive message, not forwarding to network actor: peer={} conn_actor_id={} quic_stable_id={} datagram_bytes={}",
                        conn.remote_id(),
                        conn_actor_id,
                        conn.stable_id(),
                        datagram_len
                    );*/
                    if frame.is_empty() {
                        let remote_id = conn.remote_id();
                        debug!(%remote_id, "received empty keepalive frame");
                        continue;
                    }
                    remote_frame_tx.send((conn.remote_id(), frame)).await.ok();
                    continue;
                }

                /*debug!(
                    "iroh-recv peer={} conn_actor_id={} quic_stable_id={} kind={} payload_bytes={} datagram_bytes={}",
                    conn.remote_id(),
                    conn_actor_id,
                    conn.stable_id(),
                    msg_kind,
                    payload_len,
                    datagram_len
                );*/

                if let Err(e) = sender.send(msg).await {
                    warn!("Failed to forward message to network actor: {}", e);
                    break;
                }
                /*debug!(
                    "iroh-forwarded-to-network peer={} conn_actor_id={} quic_stable_id={} kind={} payload_bytes={} datagram_bytes={}",
                    conn.remote_id(),
                    conn_actor_id,
                    conn.stable_id(),
                    msg_kind,
                    payload_len,
                    datagram_len
                );*/
                if start.elapsed().as_millis() > BACKPRESSURE_WARN_MS {
                    warn!(
                        "Direct->Network backpressure: send blocked {} ms",
                        start.elapsed().as_millis()
                    );
                }
            }
            Ok(Err(ReadError::Multiplex(err))) => {
                warn!("Multiplex error: {:?}", err);
                continue;
            }
            Ok(Err(e)) => {
                warn!("Stream read error: {:?}", e);
                info!("paths info: {:?}", conn.paths());
                info!("Read task stopped");
                warn!(
                    "Read failed after hard error, dropping connection: peer_id: {}",
                    if let Ok(peer_id) = api
                        .call(act_ok!(actor => async move { actor.conn_endpoint_id }))
                        .await
                    {
                        peer_id.to_string()
                    } else {
                        "unknown".to_string()
                    }
                );
                let _ = api.call(act_ok!(actor => actor.close())).await;
                break;
            }
            Err(e) => {
                warn!("Stream read error: timeout after {}", e);
                {
                    let mut health = health.lock().await;
                    health.consecutive_read_timeouts =
                        health.consecutive_read_timeouts.saturating_add(1);
                }

                let snapshot = api
                    .call(act_ok!(actor => async move { actor.snapshot().await }))
                    .await;
                match snapshot.map(|snapshot| snapshot.liveness) {
                    Ok(ConnLiveness::Dead) => {
                        info!("Read task stopped");
                        warn!(
                            "Read failed after timeout evidence, dropping connection: peer_id: {}",
                            if let Ok(peer_id) = api
                                .call(act_ok!(actor => async move { actor.conn_endpoint_id }))
                                .await
                            {
                                peer_id.to_string()
                            } else {
                                "unknown".to_string()
                            }
                        );
                        let _ = api.call(act_ok!(actor => actor.close())).await;
                        break;
                    }
                    Ok(liveness) => {
                        warn!(
                            "Read timeout moved connection to {:?}, keeping it for recovery: peer={} conn_actor_id={} quic_stable_id={}",
                            liveness,
                            conn.remote_id(),
                            conn_actor_id,
                            conn.stable_id()
                        );

                        warn!(
                            "Stopping read task just because it doesnt open a new conn if it is kept"
                        );
                        let _ = api.call(act_ok!(actor => actor.close())).await;
                        break;
                    }
                    Err(err) => {
                        warn!("Failed to classify timed out connection: {}", err);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
enum ReadError {
    Datagram(String),
    Multiplex(String),
    Deserialize(String),
}

impl Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Datagram(e) => write!(f, "Read error: {}", e),
            ReadError::Multiplex(e) => write!(f, "Read error: {}", e),
            ReadError::Deserialize(e) => write!(f, "Read error: {}", e),
        }
    }
}

impl std::error::Error for ReadError {}

async fn read_next_msg(conn: &Connection) -> Result<(DirectMessage, usize), ReadError> {
    let buf = conn
        .read_datagram()
        .await
        .map_err(|e| ReadError::Datagram(format!("failed to read datagram: {}", e)))?;
    let datagram_len = buf.len();
    let prefix = buf.first().copied();
    let preview_len = buf.len().min(8);
    let preview = buf[..preview_len].to_vec();
    /*debug!(
        "iroh-read-datagram peer={} quic_stable_id={} datagram_bytes={} prefix={:?} preview={:02X?}",
        conn.remote_id(),
        conn.stable_id(),
        datagram_len,
        prefix,
        preview
    );*/
    if buf.len() > 1 && buf[0] == DATAGRAM_PREFIX {
        let msg: DirectMessage = postcard::from_bytes(&buf[1..])
            .map_err(|e| {
                warn!(
                    "iroh-read-deserialize-failed peer={} quic_stable_id={} datagram_bytes={} prefix={:?} preview={:02X?} error={}",
                    conn.remote_id(),
                    conn.stable_id(),
                    datagram_len,
                    prefix,
                    preview,
                    e
                );
                ReadError::Deserialize(format!("failed to deserialize message: {}", e))
            })?;
        /*debug!(
            "iroh-read-deserialized peer={} quic_stable_id={} kind={} payload_bytes={} datagram_bytes={}",
            conn.remote_id(),
            conn.stable_id(),
            msg.kind_name(),
            msg.payload_len(),
            datagram_len
        );*/
        Ok((msg, datagram_len))
    } else {
        debug!(
            "iroh-raw-recv-unhandled peer={} datagram_bytes={} prefix={:?} preview={:02X?}",
            conn.remote_id(),
            datagram_len,
            prefix,
            preview
        );
        Err(ReadError::Multiplex(format!(
            "not meant for us: len={} prefix={:?}",
            datagram_len, prefix
        )))
    }
}

async fn resolve_addr(ep: &Endpoint, peer_id: EndpointId) -> EndpointAddr {
    let dns = DnsAddressLookup::n0_dns()
        .dns_resolver(ep.dns_resolver().unwrap().clone())
        .build();

    let relay_url = match dns.resolve(peer_id) {
        None => None,
        Some(stream) => {
            // Take the first successful result
            stream
                .filter_map(|item| item.ok()) // unwrap Result
                .find_map(|item| item.relay_urls().next().cloned())
                .await
        }
    };

    match relay_url {
        Some(url) => EndpointAddr::new(peer_id).with_relay_url(url),
        None => EndpointAddr::new(peer_id),
    }
}
