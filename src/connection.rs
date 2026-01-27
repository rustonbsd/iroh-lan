use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Duration};

use crate::{DirectMessage, auth};
use actor_helper::{Action, Actor, Handle, Receiver, act, act_ok};
use anyhow::Result;
use iroh::endpoint::{Connection, VarInt};
use iroh::{
    Endpoint, EndpointId,
    endpoint::{RecvStream, SendStream},
};
use pnet_packet::{ip::IpNextHeaderProtocols, ipv4::Ipv4Packet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{self, Instant};
use tracing::{debug, info, trace, warn};

const QUEUE_SIZE: usize = 1024 * 16;
const MAX_RECONNECTS: usize = 100;
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(200);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(3);
const BACKPRESSURE_WARN_MS: u128 = 5;
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SENDER_QUEUE: usize = 50_000;
const WRITE_CHANNEL_CAP: usize = 8_192;
const MAX_CONSECUTIVE_WRITE_ERRORS: u64 = 3;
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const QUEUE_WARN_LEN: usize = 10_000;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Conn {
    api: Handle<ConnActor, anyhow::Error>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting, // ConnActor::connect() called, waiting for connection to be established (in background)
    Idle,       // no active connection, can be connected
    Open,       // open bi directional streams
    Closed,     // connection closed by user or error
    Disconnected, // connection closed by remote peer, can be recovered within 5 retries after Closed
}

#[derive(Debug)]
struct ConnActor {
    rx: Receiver<Action<ConnActor>>,
    self_handle: Handle<ConnActor, anyhow::Error>,
    state: ConnState,

    // all of these need to be optionals so that we can create an empty
    // shell of the actor and then fill in the values later so we don't wait
    // forever in the main standalone loop for router events hanging on
    // route_packet failed
    conn: Option<Connection>,
    conn_endpoint_id: EndpointId,
    endpoint: Endpoint,
    network_secret: [u8; 64],

    last_reconnect: tokio::time::Instant,
    reconnect_backoff: Duration,
    reconnect_count: AtomicUsize,

    external_sender: tokio::sync::mpsc::Sender<DirectMessage>,

    ctrl_read_task: Option<tokio::task::JoinHandle<()>>,
    ctrl_write_task: Option<tokio::task::JoinHandle<()>>,
    ctrl_write_tx: Option<tokio::sync::mpsc::Sender<DirectMessage>>,

    data_read_task: Option<tokio::task::JoinHandle<()>>,
    data_write_task: Option<tokio::task::JoinHandle<()>>,
    data_write_tx: Option<tokio::sync::mpsc::Sender<DirectMessage>>,

    ctrl_queue_len: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    data_queue_len: std::sync::Arc<std::sync::atomic::AtomicUsize>,

    sender_queue: VecDeque<DirectMessage>,
    last_rx: Instant,
    last_tx: Instant,
    rx_count: u64,
    tx_count: u64,
    write_timeouts: u64,
    consecutive_write_errors: u64,
    dropped_ctrl: u64,
    dropped_data: u64,
}

impl Conn {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        endpoint: Endpoint,
        conn: iroh::endpoint::Connection,
        ctrl_send_stream: SendStream,
        ctrl_recv_stream: RecvStream,
        data_send_stream: SendStream,
        data_recv_stream: RecvStream,
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
        network_secret: &[u8; 64],
    ) -> Result<Self> {
        let (api, rx) = Handle::channel();
        let mut actor = ConnActor::new(
            rx,
            api.clone(),
            external_sender,
            endpoint,
            conn.remote_id(),
            Some(conn),
            Some(ctrl_send_stream),
            Some(ctrl_recv_stream),
            Some(data_send_stream),
            Some(data_recv_stream),
            network_secret,
        )
        .await;
        tokio::spawn(async move { actor.run().await });
        Ok(Self { api })
    }

    pub async fn connect(
        endpoint: Endpoint,
        endpoint_id: EndpointId,
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
        network_secret: &[u8; 64],
    ) -> Self {
        let (api, rx) = Handle::channel();
        let mut actor = ConnActor::new(
            rx,
            api.clone(),
            external_sender,
            endpoint.clone(),
            endpoint_id,
            None,
            None,
            None,
            None,
            None,
            network_secret,
        )
        .await;

        tokio::spawn(async move {
            actor.set_state(ConnState::Connecting);
            actor.run().await
        });
        let s = Self { api };

        tokio::spawn({
            let s = s.clone();
            let network_secret = *network_secret;
            async move {
                match endpoint.connect(endpoint_id, crate::Direct::ALPN).await {
                    Ok(conn) => {
                        match auth::open(&conn, &network_secret, endpoint.id(), endpoint_id).await {
                            Ok((ctrl_send, ctrl_recv, data_send, data_recv)) => {
                                let _ = s
                                    .incoming_connection(
                                        conn, ctrl_send, ctrl_recv, data_send, data_recv,
                                    )
                                    .await;
                            }
                            Err(e) => {
                                warn!("Auth failed during initial connection: {}", e);
                                let _ = s
                                    .api
                                    .call(
                                        act_ok!(actor => async move { actor.set_state(ConnState::Disconnected) }),
                                    )
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Initial connection failed: {}", e);
                        let _ = s
                            .api
                            .call(
                                act_ok!(actor => async move { actor.set_state(ConnState::Disconnected) }),
                            )
                            .await;
                    }
                }
            }
        });

        s
    }

    pub async fn get_state(&self) -> ConnState {
        if let Ok(state) = self
            .api
            .call(act_ok!(actor => async move {
                actor.state
            }))
            .await
        {
            state
        } else {
            ConnState::Closed
        }
    }

    pub async fn close(&self) -> Result<()> {
        self.api.call(act_ok!(actor => actor.close())).await
    }

    pub async fn write(&self, pkg: DirectMessage) -> Result<()> {
        self.api.call(act_ok!(actor => actor.write(pkg))).await
    }

    pub async fn incoming_connection(
        &self,
        conn: Connection,
        ctrl_send_stream: SendStream,
        ctrl_recv_stream: RecvStream,
        data_send_stream: SendStream,
        data_recv_stream: RecvStream,
    ) -> Result<()> {
        self.api
            .call(act!(actor => actor.incoming_connection(conn, ctrl_send_stream, ctrl_recv_stream, data_send_stream, data_recv_stream)))
            .await
    }
}

impl Actor<anyhow::Error> for ConnActor {
    async fn run(&mut self) -> Result<()> {
        let mut reconnect_ticker = tokio::time::interval(Duration::from_millis(500));
        reconnect_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut keepalive_ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut stats_ticker = tokio::time::interval(STATS_LOG_INTERVAL);
        stats_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("ConnActor started for peer: {}", self.conn_endpoint_id);

        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }
                _ = reconnect_ticker.tick(), if self.state != ConnState::Closed => {

                    let connecting = self.state == ConnState::Connecting;
                    let need_reconnect = !connecting && (
                        self.state == ConnState::Disconnected
                        || self.ctrl_write_task.as_ref().map(|t| t.is_finished()).unwrap_or(true)
                        || self.data_write_task.as_ref().map(|t| t.is_finished()).unwrap_or(true)
                        || self.conn.as_ref().and_then(|c| c.close_reason()).is_some()
                    );

                    if need_reconnect && self.last_reconnect.elapsed() > self.reconnect_backoff {
                        if self.reconnect_count.load(std::sync::atomic::Ordering::SeqCst) < MAX_RECONNECTS {
                            warn!("Write task finished or connection issues detected. Attempting reconnect.");
                            let _ = self.try_reconnect().await;
                        } else {
                            warn!("Max reconnects reached, closing connection to {}", self.conn_endpoint_id);
                            break;
                        }
                    }
                }
                _ = keepalive_ticker.tick(), if self.state == ConnState::Open => {
                    if let Some(tx) = &self.ctrl_write_tx {
                        match tx.try_send(DirectMessage::IDontLikeWarnings) {
                            Ok(_) => {
                                let new_len = self.ctrl_queue_len.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                if new_len > QUEUE_WARN_LEN {
                                    warn!("Stream queue length high (keepalive): {}", new_len);
                                }
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                self.dropped_ctrl = self.dropped_ctrl.saturating_add(1);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                self.dropped_ctrl = self.dropped_ctrl.saturating_add(1);
                            }
                        }
                    }
                }
                _ = stats_ticker.tick() => {
                    debug!(
                        "Conn stats: state={:?} last_rx={:?} last_tx={:?} rx_count={} tx_count={} ctrl_q={} data_q={} write_timeouts={} write_errors={} dropped_ctrl={} dropped_data={}",
                        self.state,
                        self.last_rx.elapsed(),
                        self.last_tx.elapsed(),
                        self.rx_count,
                        self.tx_count,
                        self.ctrl_queue_len.load(std::sync::atomic::Ordering::Relaxed),
                        self.data_queue_len.load(std::sync::atomic::Ordering::Relaxed),
                        self.write_timeouts,
                        self.consecutive_write_errors,
                        self.dropped_ctrl,
                        self.dropped_data
                    );
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl-C, stopping actor");
                    break
                }
            }
        }
        self.close().await;
        Ok(())
    }
}

impl ConnActor {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        rx: Receiver<Action<ConnActor>>,
        self_handle: Handle<ConnActor, anyhow::Error>,
        external_sender: tokio::sync::mpsc::Sender<DirectMessage>,
        endpoint: Endpoint,
        conn_endpoint_id: EndpointId,
        conn: Option<iroh::endpoint::Connection>,
        ctrl_send_stream: Option<SendStream>,
        mut ctrl_recv_stream: Option<RecvStream>,
        data_send_stream: Option<SendStream>,
        mut data_recv_stream: Option<RecvStream>,
        network_secret: &[u8; 64],
    ) -> Self {
        let mut ctrl_read_task = None;
        if let Some(recv) = ctrl_recv_stream.take() {
            info!("Spawning control read task immediately in new");
            let task = tokio::spawn(retry_read_loop(
                recv,
                external_sender.clone(),
                self_handle.clone(),
            ));
            ctrl_read_task = Some(task);
        }

        let mut ctrl_write_task = None;
        let mut ctrl_write_tx = None;
        let ctrl_queue_len = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if let Some(send) = ctrl_send_stream {
            info!("Spawning control write task immediately in new");
            let (tx, rx) = tokio::sync::mpsc::channel(WRITE_CHANNEL_CAP);
            let task = tokio::spawn(write_loop_bounded(
                send,
                rx,
                self_handle.clone(),
                ctrl_queue_len.clone(),
                "control",
            ));
            ctrl_write_task = Some(task);
            ctrl_write_tx = Some(tx);
        }

        let mut data_read_task = None;
        if let Some(recv) = data_recv_stream.take() {
            info!("Spawning data read task immediately in new");
            let task = tokio::spawn(retry_read_loop(
                recv,
                external_sender.clone(),
                self_handle.clone(),
            ));
            data_read_task = Some(task);
        }

        let mut data_write_task = None;
        let mut data_write_tx = None;
        let data_queue_len = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if let Some(send) = data_send_stream {
            info!("Spawning data write task immediately in new");
            let (tx, rx) = tokio::sync::mpsc::channel(WRITE_CHANNEL_CAP);
            let task = tokio::spawn(write_loop_bounded(
                send,
                rx,
                self_handle.clone(),
                data_queue_len.clone(),
                "data",
            ));
            data_write_task = Some(task);
            data_write_tx = Some(tx);
        }

        Self {
            rx,
            state: if conn.is_some()
                && ctrl_write_task.is_some()
                && ctrl_read_task.is_some()
                && data_write_task.is_some()
                && data_read_task.is_some()
            {
                ConnState::Open
            } else {
                ConnState::Disconnected
            },
            external_sender,
            ctrl_read_task,
            ctrl_write_task,
            ctrl_write_tx,
            data_read_task,
            data_write_task,
            data_write_tx,
            ctrl_queue_len,
            data_queue_len,
            sender_queue: VecDeque::with_capacity(QUEUE_SIZE),
            conn,
            endpoint,
            network_secret: *network_secret,
            last_reconnect: tokio::time::Instant::now(),
            reconnect_backoff: Duration::from_millis(100),
            conn_endpoint_id,
            self_handle,
            reconnect_count: AtomicUsize::new(0),
            last_rx: Instant::now(),
            last_tx: Instant::now(),
            rx_count: 0,
            tx_count: 0,
            write_timeouts: 0,
            consecutive_write_errors: 0,
            dropped_ctrl: 0,
            dropped_data: 0,
        }
    }

    pub fn set_state(&mut self, state: ConnState) {
        if self.state != state {
            info!(
                "Connection state transition: {:?} -> {:?}",
                self.state, state
            );
            self.state = state;
        }
    }

    pub async fn close(&mut self) {
        info!("Closing connection actor");
        self.state = ConnState::Closed;
        if let Some(conn) = self.conn.as_mut() {
            conn.close(VarInt::from_u32(400), b"Connection closed by user");
        }
        if let Some(task) = self.ctrl_read_task.take() {
            task.abort();
        }
        if let Some(task) = self.ctrl_write_task.take() {
            task.abort();
        }
        if let Some(task) = self.data_read_task.take() {
            task.abort();
        }
        if let Some(task) = self.data_write_task.take() {
            task.abort();
        }
        self.ctrl_write_tx = None;
        self.data_write_tx = None;
        self.conn = None;
    }

    pub async fn handle_write_error(&mut self) {
        self.consecutive_write_errors = self.consecutive_write_errors.saturating_add(1);
        warn!(
            "Write loop failed. consecutive_write_errors={}",
            self.consecutive_write_errors
        );
        self.ctrl_write_tx = None;
        self.data_write_tx = None;
        if let Some(task) = self.ctrl_write_task.take() {
            task.abort();
        }
        if let Some(task) = self.data_write_task.take() {
            task.abort();
        }
        if self.consecutive_write_errors >= MAX_CONSECUTIVE_WRITE_ERRORS {
            self.set_state(ConnState::Disconnected);
        }
    }

    pub async fn write(&mut self, pkg: DirectMessage) {
        let mut use_data_stream = false;
        if let DirectMessage::IpPacket(ip_pkg) = &pkg {
            let protocol = Ipv4Packet::new(ip_pkg.as_slice()).map(|p| p.get_next_level_protocol());
            if matches!(
                protocol,
                Some(IpNextHeaderProtocols::Tcp) | Some(IpNextHeaderProtocols::Udp)
            ) {
                use_data_stream = true;
            }
        }

        let target_tx = if use_data_stream {
            &self.data_write_tx
        } else {
            &self.ctrl_write_tx
        };

        if let Some(tx) = target_tx {
            trace!("Sending packet to write task");
            match tx.try_send(pkg.clone()) {
                Ok(_) => {
                    let new_len = if use_data_stream {
                        self.data_queue_len
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1
                    } else {
                        self.ctrl_queue_len
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1
                    };
                    if new_len > QUEUE_WARN_LEN {
                        warn!("Stream queue length high: {}", new_len);
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    if use_data_stream {
                        self.dropped_data = self.dropped_data.saturating_add(1);
                    } else {
                        self.dropped_ctrl = self.dropped_ctrl.saturating_add(1);
                    }
                    if (self.dropped_data + self.dropped_ctrl).is_multiple_of(1000) {
                        warn!(
                            "Write queue full, dropping packet (ctrl_dropped={}, data_dropped={})",
                            self.dropped_ctrl,
                            self.dropped_data
                        );
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(val)) => {
                    warn!("Write task channel closed, buffering packet.");
                    self.sender_queue.push_front(val);
                    while self.sender_queue.len() > MAX_SENDER_QUEUE {
                        self.sender_queue.pop_back();
                    }
                    if self.state == ConnState::Open {
                        self.set_state(ConnState::Disconnected);
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

    fn note_rx(&mut self) {
        self.last_rx = Instant::now();
        self.rx_count = self.rx_count.saturating_add(1);
    }

    fn note_tx(&mut self) {
        self.last_tx = Instant::now();
        self.tx_count = self.tx_count.saturating_add(1);
        self.consecutive_write_errors = 0;
    }

    fn inc_write_timeout(&mut self) {
        self.write_timeouts = self.write_timeouts.saturating_add(1);
    }

    pub async fn incoming_connection(
        &mut self,
        conn: Connection,
        ctrl_send_stream: SendStream,
        ctrl_recv_stream: RecvStream,
        data_send_stream: SendStream,
        data_recv_stream: RecvStream,
    ) -> Result<()> {
        info!("Incoming connection from: {}", conn.remote_id());
        if conn.close_reason().is_some() {
            warn!("Incoming connection already closed");
            self.state = ConnState::Closed;
            return Err(anyhow::anyhow!("connection closed"));
        }

        if let Some(task) = self.ctrl_read_task.take() {
            task.abort();
        }

        info!("Spawning control read task for incoming connection");
        self.ctrl_read_task = Some(tokio::spawn(retry_read_loop(
            ctrl_recv_stream,
            self.external_sender.clone(),
            self.self_handle.clone(),
        )));

        if let Some(task) = self.ctrl_write_task.take() {
            task.abort();
        }

        info!("Spawning control write task for incoming connection");
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel(WRITE_CHANNEL_CAP);
        self.ctrl_queue_len
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.ctrl_write_task = Some(tokio::spawn(write_loop_bounded(
            ctrl_send_stream,
            ctrl_rx,
            self.self_handle.clone(),
            self.ctrl_queue_len.clone(),
            "control",
        )));
        self.ctrl_write_tx = Some(ctrl_tx.clone());

        if let Some(task) = self.data_read_task.take() {
            task.abort();
        }

        info!("Spawning data read task for incoming connection");
        self.data_read_task = Some(tokio::spawn(retry_read_loop(
            data_recv_stream,
            self.external_sender.clone(),
            self.self_handle.clone(),
        )));

        if let Some(task) = self.data_write_task.take() {
            task.abort();
        }

        info!("Spawning data write task for incoming connection");
        let (data_tx, data_rx) = tokio::sync::mpsc::channel(WRITE_CHANNEL_CAP);
        self.data_queue_len
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.data_write_task = Some(tokio::spawn(write_loop_bounded(
            data_send_stream,
            data_rx,
            self.self_handle.clone(),
            self.data_queue_len.clone(),
            "data",
        )));
        self.data_write_tx = Some(data_tx.clone());

        self.conn = Some(conn);
        self.set_state(ConnState::Open);
        self.reconnect_backoff = RECONNECT_BACKOFF_BASE;
        self.consecutive_write_errors = 0;

        while let Some(msg) = self.sender_queue.pop_back() {
            match ctrl_tx.try_send(msg) {
                Ok(_) => {
                    let new_len = self.ctrl_queue_len.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if new_len > QUEUE_WARN_LEN {
                        warn!("Stream queue length high (flush): {}", new_len);
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.dropped_ctrl = self.dropped_ctrl.saturating_add(1);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.dropped_ctrl = self.dropped_ctrl.saturating_add(1);
                }
            }
        }

        Ok(())
    }

    async fn try_reconnect(&mut self) -> Result<()> {
        info!("Trying to reconnect to {}", self.conn_endpoint_id);
        if self.state == ConnState::Closed {
            warn!("Cannot reconnect, actor is closed");
            return Err(anyhow::anyhow!("actor closed for good"));
        }
        if let Some(task) = self.ctrl_read_task.take() {
            task.abort();
        }
        if let Some(task) = self.ctrl_write_task.take() {
            task.abort();
        }
        if let Some(task) = self.data_read_task.take() {
            task.abort();
        }
        if let Some(task) = self.data_write_task.take() {
            task.abort();
        }
        self.ctrl_write_tx = None;
        self.data_write_tx = None;

        self.state = ConnState::Connecting;
        let next_backoff = self.reconnect_backoff * 2;
        self.reconnect_backoff = if next_backoff > RECONNECT_BACKOFF_MAX {
            RECONNECT_BACKOFF_MAX
        } else {
            next_backoff
        };
        self.last_reconnect = tokio::time::Instant::now();

        self.conn = None;

        tokio::spawn({
            let api = self.self_handle.clone();
            let endpoint = self.endpoint.clone();
            let conn_node_id = self.conn_endpoint_id;
            let network_secret = self.network_secret;
            async move {
                debug!("Initiating reconnection to {}", conn_node_id);
                if let Ok(conn) = endpoint.connect(conn_node_id, crate::Direct::ALPN).await {
                    debug!("Reconnection successful");
                    match auth::open(&conn, &network_secret, endpoint.id(), conn_node_id).await {
                        Ok((ctrl_send, ctrl_recv, data_send, data_recv)) => {
                            let _ = api
                                .call(act!(actor => actor.incoming_connection(conn, ctrl_send, ctrl_recv, data_send, data_recv)))
                                .await;
                            let _ = api
                                .call(act_ok!(actor => async move { actor.reconnect_count.store(0, std::sync::atomic::Ordering::SeqCst) }))
                                .await;
                        }
                        Err(e) => {
                            warn!("Auth failed during reconnection: {}", e);
                            let _ = api
                                .call(
                                    act_ok!(actor => async move { actor.set_state(ConnState::Disconnected) }),
                                )
                                .await;
                        }
                    }
                } else {
                    warn!("Reconnection failed");
                    let _ = api.call(act_ok!(actor => async move { actor.reconnect_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) })).await;
                    let _ = api
                        .call(
                            act_ok!(actor => async move { actor.set_state(ConnState::Disconnected) }),
                        )
                        .await;
                }
            }
        });
        Ok(())
    }
}

async fn write_loop_bounded(
    mut stream: SendStream,
    mut rx: tokio::sync::mpsc::Receiver<DirectMessage>,
    api: Handle<ConnActor, anyhow::Error>,
    queue_len: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    label: &'static str,
) {
    info!("Write task started ({})", label);
    while let Some(msg) = rx.recv().await {
        let _ = queue_len.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
        let bytes = match postcard::to_stdvec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to serialize message: {}", e);
                continue;
            }
        };

        match time::timeout(WRITE_TIMEOUT, stream.write_u32_le(bytes.len() as u32)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                warn!("Write error (len): {}", e);
                let _ = api.call(act_ok!(actor => actor.handle_write_error())).await;
                break;
            }
            Err(_) => {
                warn!("Write timeout (len) ({})", label);
                let _ = api
                    .call(act_ok!(actor => async move { actor.inc_write_timeout(); }))
                    .await;
                let _ = api.call(act_ok!(actor => actor.handle_write_error())).await;
                break;
            }
        }

        match time::timeout(WRITE_TIMEOUT, stream.write_all(&bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                warn!("Write error (payload): {}", e);
                let _ = api.call(act_ok!(actor => actor.handle_write_error())).await;
                break;
            }
            Err(_) => {
                warn!("Write timeout (payload) ({})", label);
                let _ = api
                    .call(act_ok!(actor => async move { actor.inc_write_timeout(); }))
                    .await;
                let _ = api.call(act_ok!(actor => actor.handle_write_error())).await;
                break;
            }
        }

        let _ = api
            .call(act_ok!(actor => async move { actor.note_tx(); }))
            .await;
    }
    info!("Write task stopped ({})", label);
}

async fn retry_read_loop(
    mut stream: RecvStream,
    sender: tokio::sync::mpsc::Sender<DirectMessage>,
    api: Handle<ConnActor, anyhow::Error>,
) {
    info!("Read task started");
    loop {
        match read_next_msg(&mut stream).await {
            Ok(msg) => {
                let _ = api
                    .call(act_ok!(actor => async move { actor.note_rx(); }))
                    .await;
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
            Err(e) => {
                warn!("Stream read error: {}", e);
                break;
            }
        }
    }
    info!("Read task stopped");
}

async fn read_next_msg(stream: &mut RecvStream) -> Result<DirectMessage> {
    let len = stream.read_u32_le().await?;
    let mut buf = vec![0; len as usize];
    stream.read_exact(&mut buf).await?;
    let msg: DirectMessage = postcard::from_bytes(&buf)?;
    Ok(msg)
}
