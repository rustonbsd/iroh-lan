use std::{io::Read, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use distributed_topic_tracker::{AutoDiscoveryGossip, RecordPublisher, TopicId};
use iroh::{Endpoint, NodeAddr, SecretKey};
use iroh_blobs::store::mem::MemStore;
use iroh_docs::{NamespaceId, NamespaceSecret, protocol::Docs};
use iroh_gossip::{net::Gossip, proto::HyparviewConfig};
use sha2::Digest;
use tokio::task::JoinHandle;

use crate::{
    Direct, DirectMessage, Router,
    actor::{Actor, Handle},
    router::RouterActor,
};

#[derive(Debug, Clone)]
pub struct Builder {
    entry_name: String,
    secret_key: SecretKey,
    creator_mode: bool,
    password: String,
    tun: Option<crate::Tun>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    pub fn entry_name(mut self, entry_name: &str) -> Self {
        self.entry_name = entry_name.to_string();
        self
    }

    pub fn secret_key(mut self, secret_key: SecretKey) -> Self {
        self.secret_key = secret_key;
        self
    }

    pub fn creator_mode(mut self) -> Self {
        self.creator_mode = true;
        self
    }

    pub fn password(mut self, password: &str) -> Self {
        self.password = password.to_string();
        self
    }

    pub fn tun(mut self, tun: crate::Tun) -> Self {
        self.tun = Some(tun);
        self
    }

    pub async fn build(&self) -> Result<Router> {
        let endpoint = Endpoint::builder()
            .discovery_n0()
            .secret_key(self.secret_key.clone())
            .bind()
            .await?;

        let gossip_config = HyparviewConfig {
            neighbor_request_timeout: Duration::from_millis(2000),
            ..Default::default()
        };

        let gossip = Gossip::builder()
            .membership_config(gossip_config)
            .spawn(endpoint.clone());

        let blobs = MemStore::default();
        let docs = Docs::memory()
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;

        let (direct_connect_tx, _direct_connect_rx) = tokio::sync::broadcast::channel(1024 * 16);
        let direct = Direct::new(endpoint.clone(), direct_connect_tx.clone());

        let _router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(crate::Direct::ALPN, direct.clone())
            .accept(iroh_docs::ALPN, docs.clone())
            .accept(
                iroh_blobs::ALPN,
                iroh_blobs::BlobsProtocol::new(&blobs, endpoint.clone(), None),
            )
            .spawn();

        let topic_initials = format!("lanparty-{}", self.entry_name);
        let secret_initials = format!("{topic_initials}-{}-secret", self.password)
            .as_bytes()
            .to_vec();

        let mut topic_hasher = sha2::Sha512::new();
        topic_hasher.update("iroh-lan-topic");
        topic_hasher.update(&secret_initials);
        let topic_hash: [u8; 32] = topic_hasher.finalize()[..32].try_into()?;

        let record_publisher = RecordPublisher::new(
            TopicId::new(z32::encode(&topic_hash)),
            endpoint.node_id(),
            self.secret_key.secret().clone(),
            None,
            secret_initials,
        );
        let topic = if self.creator_mode {
            gossip
                .subscribe_and_join_with_auto_discovery_no_wait(record_publisher)
                .await?
        } else {
            gossip
                .subscribe_and_join_with_auto_discovery(record_publisher)
                .await?
        };
        let (gossip_sender, gossip_receiver) = topic.split().await?;

        /*
        if self.creator_mode {
            let mut hs = HashMap::<NodeId, Ipv4Addr>::new();
            hs.insert(endpoint.node_id(), Ipv4Addr::new(172, 22, 0, 2));
            hs
        }
        */

        let author_id = docs.author_create().await?;
        let doc = docs.open(NamespaceId::from(&topic_hash)).await?.context("open doc")?;
        let (api, rx) = Handle::<crate::router::RouterActor>::channel(1024 * 16);
        tokio::spawn(async move {
            let mut router_actor = RouterActor {
                gossip_sender,
                gossip_receiver,
                author_id,
                _docs: docs,
                doc,
                node_id: endpoint.node_id(),
                _topic: Some(topic),
                direct: Arc::new(direct),
                _keep_alive_direct_connect_reader: direct_connect_tx.subscribe(),
                direct_connect_sender: direct_connect_tx,
                _router,
                tun: None,
                rx,
                blobs,
            };
            router_actor.run().await
        });

        // ---- refactored till here below wild west till end of function ----

        /*
        while router.my_ip().await.is_none() {
            println!("Waiting to get an IP address...");
            sleep(Duration::from_secs(1)).await;
        }

        let my_ip = router
            .my_ip()
            .await
            .ok_or_else(|| anyhow::anyhow!("failed to get my IP"))?;
        println!("My IP address is {}", my_ip);

        let (remote_writer, _remote_reader) = tokio::sync::broadcast::channel(1024 * 16);
        let tun = crate::Tun::new((my_ip.octets()[2], my_ip.octets()[3]), remote_writer)?;

        router.set_tun(tun).await?;

        if !self.creator_mode {
            sleep(Duration::from_millis(1000)).await;
            let data = postcard::to_stdvec(&RouterMessage::ReqMessage(ReqMessage {
                node_id: endpoint.node_id(),
            }))?;
            router.gossip_sender.broadcast(data).await?;
        }
        */

        Ok(Router { api })
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            creator_mode: false,
            entry_name: String::default(),
            secret_key: SecretKey::generate(&mut rand::thread_rng()),
            password: String::default(),
            tun: None,
        }
    }
}

pub async fn run(
    name: &str,
    password: &str,
    creator_mode: bool,
) -> anyhow::Result<(crate::Router, JoinHandle<()>)> {
    let secret = SecretKey::generate(&mut rand::thread_rng());
    let router = if creator_mode {
        crate::Router::builder()
            .entry_name(name)
            .password(password)
            .creator_mode()
            .secret_key(secret.clone())
            .build()
            .await?
    } else {
        crate::Router::builder()
            .entry_name(name)
            .password(password)
            .secret_key(secret.clone())
            .build()
            .await?
    };

    Ok((
        router.clone(),
        tokio::spawn(async move {
            let mut direct_rx = router.subscribe_direct_connect();
            let tun = router.tun.clone().expect("tun device not set").clone();
            let mut remote_reader = tun.subscribe().await.expect("subscribe to work");
            println!("Started standalone router event loop");
            loop {
                tokio::select! {
                    Ok(tun_recv) = remote_reader.recv() => {
                        if let Ok(remote_node_id)  = router.ip_to_node_id(tun_recv.clone()).await {
                            if let Err(err) = router.direct.route_packet(remote_node_id, DirectMessage::IpPacket(tun_recv)).await {
                                tokio::spawn({
                                    let router = router.clone();
                                    async move {
                                        println!("Creating new connection to peer {}", remote_node_id);
                                        if let Ok(quic_conn) = router.direct.get_endpoint().await.connect(remote_node_id, crate::Direct::ALPN).await {
                                            let _ = router.direct.handle_connection(quic_conn).await;
                                        }
                                }});
                                println!("[ERROR] failed to route packet to {:?}", remote_node_id);
                                println!("Reason: {:?}", err);
                            }
                        }
                    }
                    Ok(direct_msg) = direct_rx.recv() => {
                        match direct_msg {
                            DirectMessage::IpPacket(ip_pkg) => {
                                let _ = tun.write(ip_pkg).await;
                            }
                        }
                    }
                }
            }
        }),
    ))
}
