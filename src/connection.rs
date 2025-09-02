use std::{collections::VecDeque, time::Duration};

use crate::{
    DirectMessage,
    actor::{Action, Actor, Handle},
};
use anyhow::Result;
use iroh::endpoint::Connection;
use iroh::{
    Endpoint,
    endpoint::{RecvStream, SendStream},
};
use pnet_packet::Packet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const QUEUE_SIZE: usize = 1024;

#[derive(Debug)]
pub struct Conn {
    api: Handle<ConnActor>,
}

#[derive(Debug)]
struct ConnActor {
    rx: tokio::sync::mpsc::Receiver<Action<ConnActor>>,

    conn: Connection,
    send_stream: SendStream,
    recv_stream: RecvStream,
    endpoint: Endpoint,

    last_reconnect: u64,
    reconnect_backoff: u64,

    external_sender: tokio::sync::broadcast::Sender<DirectMessage>,

    receiver_queue: VecDeque<DirectMessage>,
    receiver_notify: tokio::sync::Notify,

    sender_queue: VecDeque<DirectMessage>,
    sender_notify: tokio::sync::Notify,
}

impl Actor for ConnActor {
    async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                Some(action) = self.rx.recv() => {
                    action(self).await;
                }
                _ = self.send_stream.stopped() => {
                    println!("Send stream stopped");
                    let _ = self.try_reconnect().await;
                }
                stream_recv = self.recv_stream.read_u32_le() => {
                    if let Ok(frame_size) = stream_recv {
                        let _res = self.remote_read_next(frame_size).await;
                        //println!("self.recv_stream.read_u32_le(): {}", _res.is_ok())
                    }
                }
                _ = self.sender_notify.notified() => {
                    let _res = self.remote_write_next().await;
                    //println!("self.remote_write_next().await: {}", _res.is_ok());
                }
                _ = self.receiver_notify.notified() => {
                    if let Some(msg) = self.receiver_queue.back() {
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
            conn,
            send_stream,
            recv_stream,
        )
        .await?;
        tokio::spawn(async move { actor.run().await });
        Ok(Self { api })
    }

    pub async fn write(&self, pkg: DirectMessage) -> Result<()> {
        self.api.call(move |actor| Box::pin(actor.write(pkg))).await
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
        })
    }

    pub async fn write(&mut self, pkg: DirectMessage) -> Result<()> {
        let _ = self.sender_queue.push_front(pkg);
        self.sender_notify.notify_one();
        Ok(())
    }

    pub async fn incoming_connection(&mut self, conn: Connection) -> Result<()> {
        let (send_stream, recv_stream) = conn.accept_bi().await?;
        self.conn = conn;
        self.send_stream = send_stream;
        self.recv_stream = recv_stream;

        if self.conn.close_reason().is_some() {
            anyhow::bail!("connection closed")
        }

        Ok(())
    }

    async fn try_reconnect(&mut self) -> Result<()> {
        let now = tokio::time::Instant::now().elapsed().as_secs();

        if self.last_reconnect != 0 && self.last_reconnect + self.reconnect_backoff > now {
            tokio::time::sleep(Duration::from_secs(self.reconnect_backoff - (now - self.last_reconnect))).await;
            self.reconnect_backoff += 1;
        }

        self.last_reconnect = tokio::time::Instant::now().elapsed().as_secs();

        if self.conn.close_reason().is_none() {
            return Ok(());
        }
        let conn = self
            .endpoint
            .connect(self.endpoint.node_id(), crate::Direct::ALPN)
            .await?;
        let (send_stream, recv_stream) = conn.open_bi().await?;
        self.send_stream = send_stream;
        self.recv_stream = recv_stream;
        self.conn = conn;
        self.reconnect_backoff = 0;
        Ok(())
    }

    async fn remote_write_next(&mut self) -> Result<()> {
        if let Some(msg) = self.sender_queue.back() {
            match msg {
                DirectMessage::IpPacket(_) => {
                    let bytes = serde_json::to_vec(msg)?;
                    self.send_stream.write_u32_le(bytes.len() as u32).await?;
                    self.send_stream.write(bytes.as_slice()).await?;
                    let _ = self.sender_queue.pop_back();
                    Ok(())
                }
                #[allow(unreachable_patterns)]
                _ => Err(anyhow::anyhow!("unsupported message type")),
            }
        } else {
            Err(anyhow::anyhow!("no message in queue"))
        }
    }

    async fn remote_read_next(&mut self, frame_len: u32) -> Result<DirectMessage> {
        let mut buf = vec![0; frame_len as usize];
        self.recv_stream.read_exact(&mut buf).await?;

        if let Ok(pkg) = serde_json::from_slice::<DirectMessage>(&buf) {
            match pkg {
                DirectMessage::IpPacket(ip_pkg) => {
                    if let Ok(ip_pkg) = ip_pkg.to_ipv4_packet() {
                        let msg = DirectMessage::IpPacket(ip_pkg.into());
                        self.receiver_queue.push_front(msg.clone());
                        self.receiver_notify.notify_one();
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
