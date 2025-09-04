use std::{
    collections::VecDeque,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    DirectMessage,
    actor::{Action, Actor, Handle},
};
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

const QUEUE_SIZE: usize = 1024 * 16;
const MAX_RECONNECTS: u64 = 5;

#[derive(Debug)]
pub struct Conn {
    api: Handle<ConnActor>,
}

#[derive(Debug)]
struct ConnActor {
    rx: tokio::sync::mpsc::Receiver<Action<ConnActor>>,

    // all of these need to be optionals so that we can create an empty
    // shell of the actor and then fill in the values later so we don't wait
    // forever in the main standalone loop for router events hanging on 
    // route_packet failed
    conn: Connection,
    conn_node_id: NodeId,
    send_stream: SendStream,
    recv_stream: RecvStream,
    endpoint: Endpoint,

    last_reconnect: u64,
    reconnect_backoff: u64,
    external_closed: bool,

    external_sender: tokio::sync::broadcast::Sender<DirectMessage>,

    receiver_queue: VecDeque<DirectMessage>,
    receiver_notify: tokio::sync::Notify,

    sender_queue: VecDeque<DirectMessage>,
    sender_notify: tokio::sync::Notify,
}

impl Actor for ConnActor {
    async fn run(&mut self) -> Result<()> {
        let mut reconnect_count = 0;
        loop {
            tokio::select! {
                Some(action) = self.rx.recv() => {
                    action(self).await;
                }
                _ = self.send_stream.stopped() => {
                    if self.external_closed {
                        println!("Connection closed by user, stopping actor");
                        break;
                    }
                    if reconnect_count < MAX_RECONNECTS {
                        println!("Send stream stopped");
                        if self.try_reconnect().await.is_err() {
                            reconnect_count += 1;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        } else {
                            reconnect_count = 0;
                        }
                    } else {
                        break;
                    }
                }
                stream_recv = self.recv_stream.read_u32_le() => {
                    if let Ok(frame_size) = stream_recv {
                        let _res = self.remote_read_next(frame_size).await;
                        //println!("self.recv_stream.read_u32_le(): {}", _res.is_ok())
                    }
                }
                _ = self.sender_notify.notified() => {
                    while self.sender_queue.len() > 0 {
                        if self.remote_write_next().await.is_err() {
                            let _ = self.try_reconnect().await;
                            break;
                        }
                    }
                    //println!("self.remote_write_next().await: {}", _res.is_ok());
                }
                _ = self.receiver_notify.notified() => {
                    while let Some(msg) = self.receiver_queue.back() {
                        if self.external_sender.send(msg.clone()).is_err() {
                            //println!("self.external_sender.send() CLOSED");
                            return Err(anyhow::anyhow!("external sender closed"));
                        }
                        //println!("self.receiver_notify.notified()");
                        let _ = self.receiver_queue.pop_back();
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    break
                }
            }
        }
        self.external_closed = true;
        Ok(())
    }
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
            external_sender,
            endpoint,
            conn.remote_node_id()?,
            conn,
            send_stream,
            recv_stream,
        )
        .await?;
        tokio::spawn(async move { actor.run().await });
        Ok(Self { api })
    }

    pub async fn connect(
        endpoint: Endpoint,
        node_id: NodeId,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
    ) -> Result<Self> {
        let (api, rx) = Handle::<ConnActor>::channel(1024);
        let mut actor = ConnActor::connect(rx, endpoint, node_id, external_sender).await?;
        tokio::spawn(async move { actor.run().await });
        Ok(Self { api })
    }

    pub async fn close(&self) -> Result<()> {
        self.api.cast(move |actor| Box::pin(actor.close())).await
    }

    pub async fn actor_is_running(&self) -> bool {
        self.api
            .call(move |actor| Box::pin(async { Ok(actor.actor_is_running().await) }))
            .await
            .unwrap_or(false)
    }

    pub async fn write(&self, pkg: DirectMessage) -> Result<()> {
        self.api.cast(move |actor| Box::pin(actor.write(pkg))).await
    }

    pub async fn incoming_connection(&self, conn: Connection) -> Result<()> {
        self.api
            .call(move |actor| Box::pin(actor.incoming_connection(conn)))
            .await
    }
}

impl ConnActor {
    pub async fn new(
        rx: tokio::sync::mpsc::Receiver<Action<ConnActor>>,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
        endpoint: Endpoint,
        conn_node_id: NodeId,
        conn: iroh::endpoint::Connection,
        send_stream: SendStream,
        recv_stream: RecvStream,
    ) -> Result<Self> {
        Ok(Self {
            rx,
            external_sender,
            receiver_queue: VecDeque::with_capacity(QUEUE_SIZE),
            sender_queue: VecDeque::with_capacity(QUEUE_SIZE),
            conn,
            send_stream,
            recv_stream,
            endpoint,
            receiver_notify: tokio::sync::Notify::new(),
            sender_notify: tokio::sync::Notify::new(),
            last_reconnect: 0,
            reconnect_backoff: 0,
            conn_node_id,
            external_closed: false,
        })
    }

    pub async fn connect(
        rx: tokio::sync::mpsc::Receiver<Action<ConnActor>>,
        endpoint: Endpoint,
        node_id: NodeId,
        external_sender: tokio::sync::broadcast::Sender<DirectMessage>,
    ) -> Result<Self> {
        let conn = endpoint.connect(node_id, crate::Direct::ALPN).await?;
        let (send_stream, recv_stream) = conn.open_bi().await?;
        let s = Self::new(
            rx,
            external_sender,
            endpoint,
            conn.remote_node_id()?,
            conn,
            send_stream,
            recv_stream,
        )
        .await?;

        Ok(s)
    }

    pub async fn close(&mut self) {
        self.external_closed = true;
        self.conn
            .close(VarInt::from_u32(400), b"Connection closed by user");
    }

    pub async fn actor_is_running(&self) -> bool {
        self.reconnect_backoff < MAX_RECONNECTS
    }

    pub async fn write(&mut self, pkg: DirectMessage) {
        let _ = self.sender_queue.push_front(pkg);
        self.sender_notify.notify_one();
    }

    pub async fn incoming_connection(&mut self, conn: Connection) -> Result<()> {
        let (send_stream, recv_stream) = conn.accept_bi().await?;
        self.conn = conn;
        self.send_stream = send_stream;
        self.recv_stream = recv_stream;

        if self.conn.close_reason().is_some() {
            anyhow::bail!("connection closed")
        }

        // SHOULD NOT CHANGE but just for sanity
        self.conn_node_id = self.conn.remote_node_id()?;

        Ok(())
    }

    async fn try_reconnect(&mut self) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        if self.last_reconnect != 0 && now > self.last_reconnect + self.reconnect_backoff {
            tokio::time::sleep(Duration::from_secs(
                self.reconnect_backoff - (now - self.last_reconnect),
            ))
            .await;
            self.reconnect_backoff *= 3;
        }

        self.last_reconnect = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        println!("Last reconnect attempt was at {}", self.last_reconnect);

        if self.conn.close_reason().is_none() {
            return Ok(());
        }
        let conn = self
            .endpoint
            .connect(self.conn_node_id, crate::Direct::ALPN)
            .await?;
        let (send_stream, recv_stream) = conn.open_bi().await?;
        self.send_stream = send_stream;
        self.recv_stream = recv_stream;
        self.conn = conn;
        self.reconnect_backoff = 0;
        Ok(())
    }

    async fn remote_write_next(&mut self) -> Result<()> {
        let start = SystemTime::now();
        let mut wrote = 0;
        while let Some(msg) = self.sender_queue.back() {
            let bytes = postcard::to_stdvec(msg)?;
            self.send_stream.write_u32_le(bytes.len() as u32).await?;
            self.send_stream.write_all(bytes.as_slice()).await?;
            let _ = self.sender_queue.pop_back();
            wrote += 1;
            if wrote >= 256 {
                break;
            }
        }

        if !self.sender_queue.is_empty() {
            self.sender_notify.notify_one();
        }

        let end = SystemTime::now();
        let duration = end.duration_since(start).unwrap();
        //println!("write_remote: {wrote}; elapsed: {}", duration.as_millis());
        Ok(())
    }

    async fn remote_read_next(&mut self, frame_len: u32) -> Result<DirectMessage> {
        let mut buf = vec![0; frame_len as usize];

        let start = SystemTime::now();
        self.recv_stream.read_exact(&mut buf).await?;
        //println!("elapsed timer {}", tokio::time::Instant::now().elapsed().as_millis() - timer);

        if let Ok(pkg) = postcard::from_bytes(&buf) {
            match pkg {
                DirectMessage::IpPacket(ip_pkg) => {
                    if let Ok(ip_pkg) = ip_pkg.to_ipv4_packet() {
                        let msg = DirectMessage::IpPacket(ip_pkg.into());
                        self.receiver_queue.push_front(msg.clone());
                        self.receiver_notify.notify_one();
                        let end = SystemTime::now();
                        let duration = end.duration_since(start).unwrap();
                        //println!("read_remote: elapsed: {}", duration.as_millis());
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
    }
}
