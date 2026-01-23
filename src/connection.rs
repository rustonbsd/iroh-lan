use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Duration};

use crate::{DirectMessage, auth};
use actor_helper::{Action, Actor, Handle, Receiver, act, act_ok};
use anyhow::Result;
use iroh::endpoint::{Connection, VarInt};
use iroh::{
    Endpoint, EndpointId,
    endpoint::{RecvStream, SendStream},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, trace, warn};

const QUEUE_SIZE: usize = 1024 * 1024 * 16;
const MAX_RECONNECTS: usize = 5;
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(100);

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

    external_sender: tokio::sync::broadcast::Sender<DirectMessage>,

    read_task: Option<tokio::task::JoinHandle<()>>,
    write_task: Option<tokio::task::JoinHandle<()>>,
    write_tx: Option<tokio::sync::mpsc::Sender<DirectMessage>>,

    sender_queue: VecDeque<DirectMessage>,
}

impl Conn {
    pub async fn new(
        endpoint: Endpoint,
        conn: iroh::endpoint::Connection,
        send_stream: SendStream,
        recv_stream: RecvStream,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
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
            Some(send_stream),
            Some(recv_stream),
            network_secret,
        )
        .await;
        tokio::spawn(async move { actor.run().await });
        Ok(Self { api })
    }

    pub async fn connect(
        endpoint: Endpoint,
        endpoint_id: EndpointId,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
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
                if let Ok(conn) = endpoint.connect(endpoint_id, crate::Direct::ALPN).await {
                    if let Ok((send_stream, recv_stream)) =
                        auth::open(&conn, &network_secret, endpoint.id(), endpoint_id).await
                    {
                        let _ = s.incoming_connection(conn, send_stream, recv_stream).await;
                    } else {
                        let _ = s
                            .api
                            .call(
                                act_ok!(actor => async move { actor.set_state(ConnState::Closed) }),
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
        send_stream: SendStream,
        recv_stream: RecvStream,
    ) -> Result<()> {
        self.api
            .call(act!(actor => actor.incoming_connection(conn, send_stream, recv_stream)))
            .await
    }
}

impl Actor<anyhow::Error> for ConnActor {
    async fn run(&mut self) -> Result<()> {
        let mut reconnect_ticker = tokio::time::interval(Duration::from_millis(500));
        reconnect_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("ConnActor started for peer: {}", self.conn_endpoint_id);

        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }
                _ = reconnect_ticker.tick(), if self.state != ConnState::Closed => {

                    let need_reconnect = self.write_task.as_ref().map(|t| t.is_finished()).unwrap_or(true)
                        || self.conn.as_ref().and_then(|c| c.close_reason()).is_some();

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
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
        endpoint: Endpoint,
        conn_endpoint_id: EndpointId,
        conn: Option<iroh::endpoint::Connection>,
        send_stream: Option<SendStream>,
        mut recv_stream: Option<RecvStream>,
        network_secret: &[u8; 64],
    ) -> Self {
        let mut read_task = None;
        if let Some(recv) = recv_stream.take() {
            info!("Spawning read task immediately in new");
            let task = tokio::spawn(retry_read_loop(recv, external_sender.clone()));
            read_task = Some(task);
        }

        let mut write_task = None;
        let mut write_tx = None;
        if let Some(send) = send_stream {
            info!("Spawning write task immediately in new");
            let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_SIZE);
            let task = tokio::spawn(write_loop(send, rx, self_handle.clone()));
            write_task = Some(task);
            write_tx = Some(tx);
        }

        Self {
            rx,
            state: if conn.is_some() && write_task.is_some() && read_task.is_some() {
                ConnState::Open
            } else {
                ConnState::Disconnected
            },
            external_sender,
            read_task,
            write_task,
            write_tx,
            sender_queue: VecDeque::with_capacity(QUEUE_SIZE),
            conn,
            endpoint,
            network_secret: *network_secret,
            last_reconnect: tokio::time::Instant::now(),
            reconnect_backoff: Duration::from_millis(100),
            conn_endpoint_id,
            self_handle,
            reconnect_count: AtomicUsize::new(0),
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
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
        if let Some(task) = self.write_task.take() {
            task.abort();
        }
        self.write_tx = None;
        self.conn = None;
    }

    pub async fn handle_write_error(&mut self) {
        warn!("Write loop failed. Marking as disconnected.");
        self.write_tx = None;
        if let Some(task) = self.write_task.take() {
            task.abort();
        }
        self.set_state(ConnState::Disconnected);
    }

    pub async fn write(&mut self, pkg: DirectMessage) {
        if let Some(tx) = &self.write_tx {
            trace!("Sending packet to write task");
            if let Err(e) = tx.send(pkg).await {
                warn!("Failed to send to write task, buffering.");
                self.sender_queue.push_front(e.0);
                if self.state == ConnState::Open {
                    self.set_state(ConnState::Disconnected);
                }
            }
        } else {
            trace!(
                "Queueing packet for write. Queue size: {}",
                self.sender_queue.len()
            );
            self.sender_queue.push_front(pkg);
        }
    }

    pub async fn incoming_connection(
        &mut self,
        conn: Connection,
        send_stream: SendStream,
        recv_stream: RecvStream,
    ) -> Result<()> {
        info!("Incoming connection from: {}", conn.remote_id());
        if conn.close_reason().is_some() {
            warn!("Incoming connection already closed");
            self.state = ConnState::Closed;
            return Err(anyhow::anyhow!("connection closed"));
        }

        if let Some(task) = self.read_task.take() {
            task.abort();
        }

        info!("Spawning read task for incoming connection");
        self.read_task = Some(tokio::spawn(retry_read_loop(
            recv_stream,
            self.external_sender.clone(),
        )));

        if let Some(task) = self.write_task.take() {
            task.abort();
        }

        info!("Spawning write task for incoming connection");
        let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_SIZE);
        self.write_task = Some(tokio::spawn(write_loop(
            send_stream,
            rx,
            self.self_handle.clone(),
        )));
        self.write_tx = Some(tx.clone());

        self.conn = Some(conn);
        self.state = ConnState::Open;
        self.reconnect_backoff = RECONNECT_BACKOFF_BASE;

        while let Some(msg) = self.sender_queue.pop_back() {
            let _ = tx.send(msg).await;
        }

        Ok(())
    }

    async fn try_reconnect(&mut self) -> Result<()> {
        info!("Trying to reconnect to {}", self.conn_endpoint_id);
        if self.state == ConnState::Closed {
            warn!("Cannot reconnect, actor is closed");
            return Err(anyhow::anyhow!("actor closed for good"));
        }

        if let Some(task) = self.read_task.take() {
            task.abort();
        }
        if let Some(task) = self.write_task.take() {
            task.abort();
        }
        self.write_tx = None;

        self.state = ConnState::Connecting;
        self.reconnect_backoff *= 3;
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
                    if let Ok((send_stream, recv_stream)) =
                        auth::open(&conn, &network_secret, endpoint.id(), conn_node_id).await
                    {
                        let _ = api
                            .call(act!(actor => actor.incoming_connection(conn, send_stream, recv_stream)))
                            .await;
                        let _ = api.call(act_ok!(actor => async move { actor.reconnect_count.store(0, std::sync::atomic::Ordering::SeqCst) })).await;
                    } else {
                        warn!("Auth failed during reconnection");
                        let _ = api
                            .call(
                                act_ok!(actor => async move { actor.set_state(ConnState::Closed) }),
                            )
                            .await;
                    }
                } else {
                    warn!("Reconnection failed");
                    let _ = api.call(act_ok!(actor => async move { actor.reconnect_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) })).await;
                }
            }
        });
        Ok(())
    }
}

async fn write_loop(
    mut stream: SendStream,
    mut rx: tokio::sync::mpsc::Receiver<DirectMessage>,
    api: Handle<ConnActor, anyhow::Error>,
) {
    info!("Write task started");
    while let Some(msg) = rx.recv().await {
        let bytes = match postcard::to_stdvec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to serialize message: {}", e);
                continue;
            }
        };

        if let Err(e) = stream.write_u32_le(bytes.len() as u32).await {
            warn!("Write error (len): {}", e);
            let _ = api.call(act_ok!(actor => actor.handle_write_error())).await;
            break;
        }
        if let Err(e) = stream.write_all(&bytes).await {
            warn!("Write error (payload): {}", e);
            let _ = api.call(act_ok!(actor => actor.handle_write_error())).await;
            break;
        }
    }
    info!("Write task stopped");
}

async fn retry_read_loop(
    mut stream: RecvStream,
    sender: tokio::sync::broadcast::Sender<DirectMessage>,
) {
    info!("Read task started");
    loop {
        match read_next_msg(&mut stream).await {
            Ok(msg) => {
                trace!("Read message from stream, forwarding to network actor");
                if let Err(e) = sender.send(msg) {
                    warn!("Failed to forward message to network actor: {}", e);
                    break;
                }
            }
            Err(e) => {
                warn!("Stream read error: {:?}", e);
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
