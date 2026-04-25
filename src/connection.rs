use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Duration};

use crate::DirectMessage;
use actor_helper::{Action, ActorState, Handle, Receiver, act, act_ok};
use anyhow::Result;
use bytes::Bytes;
use iroh::endpoint::{Connection, VarInt};
use iroh::{Endpoint, EndpointId};
use n0_watcher::Watchable;
use tokio::time::{self};
use tracing::{debug, info, trace, warn};

const QUEUE_SIZE: usize = 1024 * 16;
const BACKPRESSURE_WARN_MS: u128 = 5;
const MAX_SENDER_QUEUE: usize = 50_000;
const WRITE_CHANNEL_CAP: usize = 8_192;
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const QUEUE_WARN_LEN: usize = 10_000;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const CONNECTING_TIMEOUT: Duration = Duration::from_secs(20);
const DATAGRAM_PREFIX: u8 = 0x43;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conn {
    api: Handle<ConnActor, anyhow::Error>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting, // ConnActor::connect() called, waiting for connection to be established (in background)
    Open,       // open bi directional streams
    Closed,     // connection closed by user or error
    Disconnected, // connection closed by remote peer, can be recovered within 5 retries after Closed
    ClosedAndStopped, // connection closed and actor stopped, no recovery possible
}

#[derive(Debug)]
struct ConnActor {
    self_handle: Option<Handle<ConnActor, anyhow::Error>>,
    state: Watchable<ConnState>,

    // all of these need to be optionals so that we can create an empty
    // shell of the actor and then fill in the values later so we don't wait
    // forever in the main standalone loop for router events hanging on
    // route_packet failed
    conn: Option<Connection>,
    conn_endpoint_id: EndpointId,

    external_sender: tokio::sync::mpsc::Sender<DirectMessage>,

    write_task: Option<tokio::task::JoinHandle<()>>,
    write_tx: Option<tokio::sync::mpsc::Sender<DirectMessage>>,

    read_task: Option<tokio::task::JoinHandle<()>>,

    queue_len: Arc<std::sync::atomic::AtomicUsize>,
    dropped_packets: Arc<AtomicUsize>,

    sender_queue: VecDeque<DirectMessage>,
    rx_count: Arc<AtomicUsize>,
    tx_count: Arc<AtomicUsize>,
    write_timeouts: Arc<AtomicUsize>,
    consecutive_write_errors: Arc<AtomicUsize>,
}

impl Conn {
    pub async fn accept_connection(
        conn: iroh::endpoint::Connection,
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
    ) -> Result<Self> {
        let (api, _) = Handle::spawn_with(
            ConnActor::new(external_sender, conn.remote_id()),
            |mut actor, rx| async move { actor.run(rx).await },
        );
        let s = Self { api };
        let self_handle = s.api.clone();
        s.api
            .call(act_ok!(actor => async move {
                actor.state.set(ConnState::Connecting).ok();
                actor.self_handle = Some(self_handle);
            }))
            .await?;

        /*
        if let Err(err) = handshake(conn.clone(), CONNECTING_TIMEOUT).await {
            warn!("Handshake failed for {}: {}", conn.remote_id(), err);
            s.drop().await;
            anyhow::bail!("Handshake failed: {}", err);
        }
        */

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
    ) -> Result<Self> {
        let (api, _) = Handle::spawn_with(
            ConnActor::new(external_sender, remote_endpoint_id),
            |mut actor, rx| async move { actor.run(rx).await },
        );
        let s = Self { api };
        let self_handle = s.api.clone();
        s.api
            .call(act_ok!(actor => async move {
                actor.state.set(ConnState::Connecting).ok();
                actor.self_handle = Some(self_handle);
            }))
            .await?;

        let conn = match tokio::time::timeout(
            CONNECTING_TIMEOUT,
            endpoint.connect(remote_endpoint_id, crate::Direct::ALPN),
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

        /*
        if let Err(err) = handshake(conn.clone(), CONNECTING_TIMEOUT).await {
            warn!("Handshake failed for {}: {:?}", conn.remote_id(), err);
            s.drop().await;
            anyhow::bail!("Handshake failed: {}", err);
        }
        */

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

    pub async fn get_state(&self) -> Watchable<ConnState> {
        if let Ok(state) = self
            .api
            .call(act_ok!(actor => async move {
                actor.state.clone()
            }))
            .await
        {
            state
        } else {
            Watchable::new(ConnState::ClosedAndStopped)
        }
    }

    /*
    pub async fn get_conn(&self) -> Option<Connection> {
        self.api
            .call(act_ok!(actor => async {
                actor.conn.clone()
            }))
            .await
            .unwrap_or_default()
    } */

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
                actor.state.set(ConnState::ClosedAndStopped).ok();
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
            if self.state.get() == ConnState::ClosedAndStopped {
                debug!(
                    "ConnActor for {} is in ClosedAndStopped state, exiting run loop",
                    self.conn_endpoint_id
                );
                break;
            }
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                    action(self).await;
                }
                /*_ = reconnect_ticker.tick(), if self.state != ConnState::Closed => {

                    let need_reconnect = !matches!(self.state, ConnState::Open | ConnState::Connecting) &&
                        (self.write_task.as_ref().map(|t| t.is_finished()).unwrap_or(false)
                        || self.read_task.as_ref().map(|t| t.is_finished()).unwrap_or(false)
                        || self.conn.as_ref().and_then(|c| c.close_reason()).is_some()
                        || self.closed_task.as_ref().map(|t| t.is_finished()).unwrap_or(false)
                    );

                    if need_reconnect && self.last_reconnect.elapsed() > self.reconnect_backoff {
                        if self.reconnect_count.load(Ordering::SeqCst) < MAX_RECONNECTS {
                            warn!("Write task finished or connection issues detected. Attempting reconnect.");
                            let _ = self.try_reconnect().await;
                        } else {
                            warn!("Max reconnects reached, closing connection to {}", self.conn_endpoint_id);
                            break;
                        }
                    }
                }*/
                _ = keepalive_ticker.tick(), if self.state.get() == ConnState::Open => {
                    if let Some(tx) = &self.write_tx {
                        match tx.try_send(DirectMessage::IDontLikeWarnings) {
                            Ok(_) => {
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
                        "Conn stats: endpoint_id={} state={:?} rx_count={} tx_count={} queue_len={} write_timeouts={} write_errors={} dropped_packets={}",
                        self.conn_endpoint_id,
                        self.state.get(),
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
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
        conn_endpoint_id: EndpointId,
    ) -> Self {
        Self {
            state: Watchable::new(ConnState::Connecting),
            external_sender,
            read_task: None,
            write_task: None,
            write_tx: None,
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
        }
    }

    pub async fn close(&mut self) {
        if matches!(
            self.state.get(),
            ConnState::Disconnected | ConnState::ClosedAndStopped | ConnState::Closed
        ) {
            return;
        }

        info!("Closing connection actor");
        if let Some(conn) = self.conn.as_mut() {
            conn.close(VarInt::from_u32(400), b"Connection closed by user");
        }
        self.conn = None;
        self.state.set(ConnState::ClosedAndStopped).ok();

        if let Some(task) = self.read_task.take() {
            task.abort();
        }
        if let Some(task) = self.write_task.take() {
            task.abort();
        }
    }

    pub async fn write(&mut self, pkg: DirectMessage) {
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
                    if self.state.get() == ConnState::Open {
                        self.state.set(ConnState::Disconnected).ok();
                    }
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
            self.state.set(ConnState::Disconnected).ok();
            return Err(anyhow::anyhow!("connection closed"));
        }

        /*
        let mut ack_iter = 0u8;
        match conn.side() {
            iroh::endpoint::Side::Client => {
                iroh_auth::send_and_ack(
                    &conn,
                    &mut ack_iter,
                    Bytes::copy_from_slice(&[42]),
                    Duration::from_secs(5),
                )
                .await?;
                info!(
                    "Performing connection handshake for #2: {}",
                    conn.remote_id()
                );
                let recv =
                    iroh_auth::read_and_ack(&conn, &mut ack_iter, Duration::from_secs(10)).await?;
                info!(
                    "Performing connection handshake for #3: {}",
                    conn.remote_id()
                );
                if recv.len() != 1 || recv[0] != 42 {
                    warn!("Handshake failed: invalid ack from client");
                    return Err(anyhow::anyhow!("handshake failed"));
                }
            }
            iroh::endpoint::Side::Server => {
                let recv =
                    iroh_auth::read_and_ack(&conn, &mut ack_iter, Duration::from_secs(10)).await?;
                info!(
                    "Performing connection handshake for #2: {}",
                    conn.remote_id()
                );
                if recv.len() != 1 || recv[0] != 42 {
                    warn!("Handshake failed: invalid ack from server");
                    return Err(anyhow::anyhow!("handshake failed"));
                }
                iroh_auth::send_and_ack(
                    &conn,
                    &mut ack_iter,
                    Bytes::copy_from_slice(&[42]),
                    Duration::from_secs(5),
                )
                .await?;
                info!(
                    "Performing connection handshake for #3: {}",
                    conn.remote_id()
                );
            }
        } */

        info!("Spawning read task for incoming connection");
        let rx_count = self.rx_count.clone();
        self.read_task = Some(tokio::spawn(retry_read_loop(
            conn.clone(),
            self.external_sender.clone(),
            self_handle.clone(),
            rx_count,
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
            self.queue_len.clone(),
            "main",
            tx_count,
            write_timeouts,
        )));
        self.write_tx = Some(tx.clone());

        self.conn = Some(conn);
        self.consecutive_write_errors.store(0, Ordering::Relaxed);
        self.rx_count.store(0, Ordering::Relaxed);
        self.tx_count.store(0, Ordering::Relaxed);

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
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        self.state.set(ConnState::Open).ok();

        Ok(())
    }
}

async fn write_loop_bounded(
    conn: Connection,
    mut rx: tokio::sync::mpsc::Receiver<DirectMessage>,
    api: Handle<ConnActor, anyhow::Error>,
    queue_len: Arc<std::sync::atomic::AtomicUsize>,
    label: &'static str,
    tx_count: Arc<AtomicUsize>,
    write_timeout: Arc<AtomicUsize>,
) {
    info!("Write task started ({})", label);
    while let Some(msg) = rx.recv().await {
        let _ = queue_len.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_sub(1))
        });
        let bytes = match postcard::to_stdvec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to serialize message: {}", e);
                continue;
            }
        };
        let mut buf = vec![DATAGRAM_PREFIX];
        buf.extend_from_slice(&bytes);

        let mut retries = 0;
        while retries < 1 {
            match time::timeout(
                KEEPALIVE_INTERVAL * 5,
                conn.send_datagram_wait(Bytes::from(buf.clone())),
            )
            .await
            {
                Ok(Ok(())) => break,
                Ok(Err(err)) => {
                    warn!("Write error (frame): {}, {:?}, retrying...", err, err);
                    retries += 1;
                    time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => {
                    warn!("Write error timeout (frame)");
                    write_timeout.fetch_add(1, Ordering::SeqCst);
                    time::sleep(Duration::from_millis(100)).await;
                    break;
                }
            }
        }

        if retries == 1 {
            warn!(
                "Write failed after 3 retries, dropping connection: peer_id: {}",
                if let Ok(peer_id) = api
                    .call(act_ok!(actor => async move { actor.conn_endpoint_id }))
                    .await
                {
                    peer_id.to_string()
                } else {
                    "unknown".to_string()
                }
            );
            info!("Write task stopped ({})", label);
            let _ = api.call(act_ok!(actor => actor.close())).await;
            break;
        }

        tx_count.fetch_add(1, Ordering::SeqCst);
    }
}

async fn retry_read_loop(
    conn: Connection,
    sender: tokio::sync::mpsc::Sender<DirectMessage>,
    api: Handle<ConnActor, anyhow::Error>,
    rx_count: Arc<AtomicUsize>,
) {
    info!("Read task started");
    let mut retries = 0;
    while retries < 1 {
        match tokio::time::timeout(KEEPALIVE_INTERVAL * 5, read_next_msg(&conn)).await {
            Ok(Ok(msg)) => {
                retries = 0;
                rx_count.fetch_add(1, Ordering::SeqCst);
                trace!("Read message from stream, forwarding to network actor");
                let start = std::time::Instant::now();
                if let Err(e) = sender.send(msg).await {
                    warn!("Failed to forward message to network actor: {}", e);
                    break;
                }
                if start.elapsed().as_millis() > BACKPRESSURE_WARN_MS {
                    warn!(
                        "Direct->Network backpressure: send blocked {} ms",
                        start.elapsed().as_millis()
                    );
                }
            }
            Ok(Err(ReadError::Multiplex(_))) => {
                continue;
            }
            Ok(Err(e)) => {
                warn!("Stream read error: {:?}", e);
                retries += 1;
            }
            Err(e) => {
                warn!("Stream read error: timeout after {}", e);
                retries += 1;
            }
        }
    }

    info!("Read task stopped");
    if retries >= 1 {
        warn!(
            "Read failed after 3 retries, dropping connection: peer_id: {}",
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

async fn read_next_msg(conn: &Connection) -> Result<DirectMessage, ReadError> {
    let buf = conn
        .read_datagram()
        .await
        .map_err(|e| ReadError::Datagram(format!("failed to read datagram: {}", e)))?;
    if buf.len() > 1 && buf[0] == DATAGRAM_PREFIX {
        let msg: DirectMessage = postcard::from_bytes(&buf[1..])
            .map_err(|e| ReadError::Deserialize(format!("failed to deserialize message: {}", e)))?;
        Ok(msg)
    } else {
        Err(ReadError::Multiplex(format!(
            "not meant for us: prefix={:X}",
            buf[0]
        )))
    }
}
