use std::{
    collections::VecDeque,
    sync::atomic::AtomicUsize,
    time::{Duration, SystemTime},
};

use crate::DirectMessage;
use actor_helper::{Action, Actor, Handle, act, act_ok};
use anyhow::Result;
use iroh::{
    Endpoint,
    endpoint::{RecvStream, SendStream},
};
use iroh::{
    NodeId,
    endpoint::{Connection, VarInt},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

const QUEUE_SIZE: usize = 1024 * 16;
const MAX_RECONNECTS: usize = 5;
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Conn {
    api: Handle<ConnActor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting, // ConnActor::connect() called, waiting for connection to be established (in background)
    Open,       // open bi directional streams
    Closed,     // connection closed by user or error
    Disconnected, // connection closed by remote peer, can be recovered within 5 retries after Closed
}

#[derive(Debug)]
struct ConnActor {
    rx: tokio::sync::mpsc::Receiver<Action<ConnActor>>,
    self_handle: Handle<ConnActor>,
    state: ConnState,

    // all of these need to be optionals so that we can create an empty
    // shell of the actor and then fill in the values later so we don't wait
    // forever in the main standalone loop for router events hanging on
    // route_packet failed
    conn: Option<Connection>,
    conn_node_id: NodeId,
    send_stream: Option<SendStream>,
    recv_stream: Option<RecvStream>,
    endpoint: Endpoint,

    last_reconnect: tokio::time::Instant,
    reconnect_backoff: Duration,
    reconnect_count: AtomicUsize,

    external_sender: tokio::sync::broadcast::Sender<DirectMessage>,

    receiver_queue: VecDeque<DirectMessage>,
    receiver_notify: tokio::sync::Notify,

    sender_queue: VecDeque<DirectMessage>,
    sender_notify: tokio::sync::Notify,
}

impl Conn {
    pub async fn new(
        endpoint: Endpoint,
        conn: iroh::endpoint::Connection,
        send_stream: SendStream,
        recv_stream: RecvStream,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
    ) -> Result<Self> {
        let (api, rx) = Handle::<ConnActor>::channel(1024);
        let mut actor = ConnActor::new(
            rx,
            api.clone(),
            external_sender,
            endpoint,
            conn.remote_node_id()?,
            Some(conn),
            Some(send_stream),
            Some(recv_stream),
        )
        .await;
        tokio::spawn(async move { actor.run().await });
        Ok(Self { api })
    }

    pub async fn connect(
        endpoint: Endpoint,
        node_id: NodeId,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
    ) -> Self {
        let (api, rx) = Handle::<ConnActor>::channel(1024);
        let mut actor = ConnActor::new(
            rx,
            api.clone(),
            external_sender,
            endpoint.clone(),
            node_id,
            None,
            None,
            None,
        )
        .await;

        tokio::spawn(async move {
            actor.set_state(ConnState::Connecting);
            actor.run().await
        });
        let s = Self { api };

        tokio::spawn({
            let s = s.clone();
            async move {
                if let Ok(conn) = endpoint.connect(node_id, crate::Direct::ALPN).await {
                    let _ = s.incoming_connection(conn, false).await;
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

    pub async fn incoming_connection(&self, conn: Connection, accept_not_open: bool) -> Result<()> {
        self.api
            .call(act!(actor => actor.incoming_connection(conn, accept_not_open)))
            .await
    }
}

impl Actor for ConnActor {
    async fn run(&mut self) -> Result<()> {
        let mut reconnect_ticker = tokio::time::interval(Duration::from_millis(500));
        let mut notification_ticker = tokio::time::interval(Duration::from_millis(500));

        reconnect_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        notification_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(action) = self.rx.recv() => {
                    action(self).await;
                }
                _ = reconnect_ticker.tick(), if self.state != ConnState::Closed => {

                    let need_reconnect = self.send_stream.is_none()
                        || self.conn.as_ref().and_then(|c| c.close_reason()).is_some();

                    if need_reconnect && self.last_reconnect.elapsed() > self.reconnect_backoff {
                        if self.reconnect_count.load(std::sync::atomic::Ordering::SeqCst) < MAX_RECONNECTS {
                            warn!("Send stream stopped");
                            let _ = self.try_reconnect().await;
                        } else {
                            warn!("Max reconnects reached, closing connection to {}", self.conn_node_id);
                            break;
                        }
                    }
                }
                _ = notification_ticker.tick(), if self.state != ConnState::Closed
                        && (!self.sender_queue.is_empty()
                            || self.receiver_queue.is_empty()) => {

                    if !self.sender_queue.is_empty() {
                        self.sender_notify.notify_one();
                    }
                    if !self.receiver_queue.is_empty() {
                        self.receiver_notify.notify_one();
                    }
                }
                stream_recv = async {
                    let recv = self.recv_stream.as_mut().expect("checked in if via self.recv_stream.is_some()");
                    recv.read_u32_le().await
                }, if self.state != ConnState::Closed && self.recv_stream.is_some() => {
                    if let Ok(frame_size) = stream_recv {
                        let _res = self.remote_read_next(frame_size).await;
                    }
                }
                _ = self.sender_notify.notified(), if self.conn.is_some() && self.state == ConnState::Open => {
                    while !self.sender_queue.is_empty() {
                        if self.remote_write_next().await.is_err() {
                            warn!("Failed to write to remote, will attempt to reconnect");
                            self.set_state(ConnState::Disconnected);
                            break;
                        }
                    }
                }
                _ = self.receiver_notify.notified(), if self.conn.is_some() && self.state != ConnState::Closed => {

                    while let Some(msg) = self.receiver_queue.pop_back() {
                        if self.external_sender.send(msg.clone()).is_err() {
                            warn!("No active receivers for incoming messages");
                            self.set_state(ConnState::Disconnected);
                            break;
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
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
        rx: tokio::sync::mpsc::Receiver<Action<ConnActor>>,
        self_handle: Handle<ConnActor>,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
        endpoint: Endpoint,
        conn_node_id: NodeId,
        conn: Option<iroh::endpoint::Connection>,
        send_stream: Option<SendStream>,
        recv_stream: Option<RecvStream>,
    ) -> Self {
        Self {
            rx,
            state: if conn.is_some() && send_stream.is_some() && recv_stream.is_some() {
                ConnState::Open
            } else {
                ConnState::Disconnected
            },
            external_sender,
            receiver_queue: VecDeque::with_capacity(QUEUE_SIZE),
            sender_queue: VecDeque::with_capacity(QUEUE_SIZE),
            conn,
            send_stream,
            recv_stream,
            endpoint,
            receiver_notify: tokio::sync::Notify::new(),
            sender_notify: tokio::sync::Notify::new(),
            last_reconnect: tokio::time::Instant::now(),
            reconnect_backoff: Duration::from_millis(100),
            conn_node_id,
            self_handle,
            reconnect_count: AtomicUsize::new(0),
        }
    }

    pub fn set_state(&mut self, state: ConnState) {
        self.state = state;
    }

    pub async fn close(&mut self) {
        self.state = ConnState::Closed;
        if let Some(conn) = self.conn.as_mut() {
            conn.close(VarInt::from_u32(400), b"Connection closed by user");
        }
        self.conn = None;
        self.send_stream = None;
        self.recv_stream = None;
    }

    pub async fn write(&mut self, pkg: DirectMessage) {
        self.sender_queue.push_front(pkg);
        self.sender_notify.notify_one();
    }

    pub async fn incoming_connection(
        &mut self,
        conn: Connection,
        accept_not_open: bool,
    ) -> Result<()> {
        let (send_stream, recv_stream) = if accept_not_open {
            conn.accept_bi().await?
        } else {
            conn.open_bi().await?
        };

        if conn.close_reason().is_some() {
            self.state = ConnState::Closed;
            return Err(anyhow::anyhow!("connection closed"));
        }

        self.conn = Some(conn);
        self.send_stream = Some(send_stream);
        self.recv_stream = Some(recv_stream);
        self.state = ConnState::Open;
        self.sender_notify.notify_one();
        self.receiver_notify.notify_one();
        self.reconnect_backoff = RECONNECT_BACKOFF_BASE;

        // SHOULD NOT CHANGE but just for sanity
        //self.conn_node_id = self.conn.clone().expect("new_conn").remote_node_id()?;

        Ok(())
    }

    async fn try_reconnect(&mut self) -> Result<()> {
        if self.state == ConnState::Closed {
            return Err(anyhow::anyhow!("actor closed for good"));
        }

        self.state = ConnState::Connecting;
        self.reconnect_backoff *= 3;
        self.last_reconnect = tokio::time::Instant::now();

        self.send_stream = None;
        self.recv_stream = None;
        self.conn = None;

        tokio::spawn({
            let api = self.self_handle.clone();
            let endpoint = self.endpoint.clone();
            let conn_node_id = self.conn_node_id;
            async move {
                if let Ok(conn) = endpoint.connect(conn_node_id, crate::Direct::ALPN).await {
                    let _ = api
                        .call(act!(actor => actor.incoming_connection(conn, false)))
                        .await;
                    let _ = api.call(act_ok!(actor => async move { actor.reconnect_count.store(0, std::sync::atomic::Ordering::SeqCst) })).await;
                } else {
                    let _ = api.call(act_ok!(actor => async move { actor.reconnect_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) })).await;
                }
            }
        });
        Ok(())
    }

    async fn remote_write_next(&mut self) -> Result<()> {
        let start = SystemTime::now();
        let mut wrote = 0;
        if let Some(send_stream) = &mut self.send_stream {
            while let Some(msg) = self.sender_queue.back() {
                let bytes = postcard::to_stdvec(msg)?;
                send_stream.write_u32_le(bytes.len() as u32).await?;
                send_stream.write_all(bytes.as_slice()).await?;
                let _ = self.sender_queue.pop_back();
                wrote += 1;
                if wrote >= 256 {
                    break;
                }
            }
        } else {
            return Err(anyhow::anyhow!("no send stream"));
        }

        if !self.sender_queue.is_empty() {
            self.sender_notify.notify_one();
        }

        let end = SystemTime::now();
        let duration = end.duration_since(start).unwrap();
        debug!("write_remote: {wrote}; elapsed: {}", duration.as_millis());
        Ok(())
    }

    async fn remote_read_next(&mut self, frame_len: u32) -> Result<DirectMessage> {
        if let Some(recv_stream) = &mut self.recv_stream {
            let mut buf = vec![0; frame_len as usize];

            let start = SystemTime::now();
            recv_stream.read_exact(&mut buf).await?;

            if let Ok(pkg) = postcard::from_bytes(&buf) {
                match pkg {
                    DirectMessage::IpPacket(ip_pkg) => {
                        if let Ok(ip_pkg) = ip_pkg.to_ipv4_packet() {
                            let msg = DirectMessage::IpPacket(ip_pkg.into());
                            self.receiver_queue.push_front(msg.clone());
                            self.receiver_notify.notify_one();
                            let end = SystemTime::now();
                            let duration = end.duration_since(start).unwrap();
                            debug!("read_remote: elapsed: {}", duration.as_millis());
                            Ok(msg)
                        } else {
                            Err(anyhow::anyhow!("failed to convert to IPv4 packet"))
                        }
                    }
                    #[allow(unreachable_patterns)]
                    _ => Err(anyhow::anyhow!("unsupported message type")),
                }
            } else {
                Err(anyhow::anyhow!("failed to deserialize message"))
            }
        } else {
            Err(anyhow::anyhow!("no recv stream"))
        }
    }
}
