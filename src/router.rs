use std::{collections::BTreeMap, net::Ipv4Addr, time::Duration};

use distributed_topic_tracker::{
    AutoDiscoveryGossip, GossipReceiver, GossipSender, RecordPublisher, Topic, TopicId,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use futures::StreamExt;
use iroh_blobs::store::mem::MemStore;
use iroh_docs::{
    AuthorId, Entry, NamespaceSecret,
    api::Doc,
    protocol::Docs,
    store::{Query, QueryBuilder, SingleLatestPerKeyQuery},
};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_gossip::net::Gossip;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tracing::{debug, info};

use actor_helper::{Action, Actor, Handle, act, act_ok};

#[derive(Debug, Clone)]
pub struct Builder {
    entry_name: String,
    secret_key: SecretKey,
    creator_mode: bool,
    password: String,
    endpoint: Option<Endpoint>,
    gossip: Option<Gossip>,
    docs: Option<Docs>,
    blobs: MemStore,
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

    pub fn endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    pub fn gossip(mut self, gossip: Gossip) -> Self {
        self.gossip = Some(gossip);
        self
    }

    pub fn docs(mut self, docs: Docs) -> Self {
        self.docs = Some(docs);
        self
    }

    pub fn blobs(mut self, blobs: MemStore) -> Self {
        self.blobs = blobs;
        self
    }

    pub async fn build(&self) -> Result<Router> {
        let endpoint = if let Some(ep) = &self.endpoint {
            ep.clone()
        } else {
            bail!("endpoint must be set");
        };
        let gossip = if let Some(g) = &self.gossip {
            g.clone()
        } else {
            bail!("gossip must be set");
        };
        let docs = if let Some(d) = &self.docs {
            d.clone()
        } else {
            bail!("docs must be set");
        };
        let blobs = self.blobs.clone();

        let topic_initials = format!("lanparty-{}", self.entry_name);
        let secret_initials = format!("{topic_initials}-{}-secret", self.password)
            .as_bytes()
            .to_vec();

        let mut topic_hasher = sha2::Sha512::new();
        topic_hasher.update("iroh-lan-topic");
        topic_hasher.update(&secret_initials);
        let topic_hash: [u8; 32] = topic_hasher.finalize()[..32].try_into()?;

        let signing_key = SigningKey::from_bytes(&self.secret_key.to_bytes());
        let record_publisher = RecordPublisher::new(
            TopicId::new(z32::encode(&topic_hash)),
            VerifyingKey::from_bytes(endpoint.id().as_bytes())?,
            signing_key,
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

        let doc_peers = gossip_receiver
            .neighbors()
            .await
            .iter()
            .map(|pub_key| EndpointAddr::new(*pub_key))
            .collect::<Vec<_>>();

        debug!("[Doc peers]: {:?}", doc_peers);

        let author_id = docs.author_create().await?;
        let doc = docs
            .import(iroh_docs::DocTicket {
                capability: iroh_docs::Capability::Write(NamespaceSecret::from_bytes(&topic_hash)),
                nodes: doc_peers.clone(),
            })
            .await?;

        while match doc.get_sync_peers().await {
            Ok(Some(peers)) => peers.is_empty(),
            Ok(None) => true,
            Err(_) => true,
        } {
            debug!("Waiting for doc to be ready...");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let (api, rx) = Handle::channel();
        tokio::spawn(async move {
            let mut router_actor = RouterActor {
                _gossip_sender: gossip_sender,
                _gossip_receiver: gossip_receiver,
                author_id,
                _docs: docs,
                doc,
                endpoint_id: endpoint.id(),
                _topic: Some(topic),
                rx,
                blobs,
                my_ip: RouterIp::NoIp,
            };
            router_actor.run().await
        });

        Ok(Router { api })
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            creator_mode: false,
            entry_name: String::default(),
            secret_key: SecretKey::generate(&mut rand::rng()),
            password: String::default(),
            endpoint: None,
            gossip: None,
            docs: None,
            blobs: MemStore::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Router {
    api: Handle<RouterActor, anyhow::Error>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouterIp {
    NoIp,
    AquiringIp(IpCandidate, tokio::time::Instant),
    AssignedIp(Ipv4Addr),
}

#[derive(Debug)]
struct RouterActor {
    pub(crate) rx: actor_helper::Receiver<Action<RouterActor>>,

    pub _gossip_sender: GossipSender,
    pub _gossip_receiver: GossipReceiver,

    pub(crate) blobs: MemStore,
    pub(crate) _docs: Docs,
    pub(crate) doc: Doc,
    pub(crate) author_id: AuthorId,

    pub endpoint_id: EndpointId,
    pub(crate) _topic: Option<Topic>,

    pub my_ip: RouterIp,
}

impl Router {
    pub fn builder() -> Builder {
        Builder::new()
    }

    pub async fn get_ip_state(&self) -> Result<RouterIp> {
        self.api
            .call(act_ok!(actor => async move {
                    actor.my_ip.clone()
            }))
            .await
    }

    pub async fn get_node_id(&self) -> Result<EndpointId> {
        self.api
            .call(act_ok!(actor => async move {
                    actor.endpoint_id
            }))
            .await
    }

    pub async fn get_ip_from_endpoint_id(&self, endpoint_id: EndpointId) -> Result<Ipv4Addr> {
        self.api
            .call(act!(actor => actor.get_ip_from_endpoint_id(endpoint_id)))
            .await
    }

    pub async fn get_endpoint_id_from_ip(&self, ip: Ipv4Addr) -> Result<EndpointId> {
        self.api
            .call(act!(actor => actor.get_endpoint_id_from_ip(ip)))
            .await
    }

    pub async fn get_peers(&self) -> Result<Vec<(EndpointId, Option<Ipv4Addr>)>> {
        self.api
            .call(act!(actor => async move {
                let mut map: BTreeMap<EndpointId, Option<Ipv4Addr>> = BTreeMap::new();

                if let Ok(assignments) = actor.read_all_ip_assignments().await {
                    for a in assignments {
                        map.insert(a.endpoint_id, Some(a.ip));
                    }
                }

                if let Ok(cands) = actor.read_all_ip_candidates().await {
                    for c in cands {
                        map.entry(c.endpoint_id).or_insert(None);
                    }
                }

                map.remove(&actor.endpoint_id);

                Ok(map.into_iter().collect())
            }))
            .await
    }

    pub async fn close(&self) -> Result<()> {
        self.api.call(act!(actor => actor.doc.close())).await
    }
}

impl Actor<anyhow::Error> for RouterActor {
    async fn run(&mut self) -> Result<()> {
        let mut ip_tick = tokio::time::interval(Duration::from_millis(500));
        ip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    break
                }

                // advance ip state machine
                _ = ip_tick.tick(), if !matches!(self.my_ip, RouterIp::AssignedIp(_)) => {
                    if let Ok(true) = self.advance_ip_state().await {
                        info!("Acquired IP: {:?}", self.my_ip);
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpAssignment {
    pub ip: Ipv4Addr,
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpCandidate {
    pub ip: Ipv4Addr,
    pub endpoint_id: EndpointId,
}

async fn entry_to_value<S: for<'a> Deserialize<'a>>(
    blobs: MemStore,
    entry: &Entry,
) -> anyhow::Result<S> {
    let b = blobs.get_bytes(entry.content_hash()).await?;
    postcard::from_bytes::<S>(&b).context("failed to deserialize value")
}

fn query(q: impl Into<String>) -> impl Into<Query> {
    QueryBuilder::<SingleLatestPerKeyQuery>::default()
        .key_exact(q.into())
        .build()
}

fn query_prefix(q: impl Into<String>) -> impl Into<Query> {
    QueryBuilder::<SingleLatestPerKeyQuery>::default()
        .key_prefix(q.into())
        .build()
}

fn key_ip_assigned(ip: Ipv4Addr) -> String {
    format!("/assigned/ip/{ip}")
}

fn key_ip_assigned_prefix() -> String {
    "/assigned/ip/".to_string()
}

fn key_ip_candidate(ip: Ipv4Addr, endpoint_id: EndpointId) -> String {
    format!("/candidates/ip/{ip}/{endpoint_id}")
}

fn key_ip_candidate_prefix() -> String {
    "/candidates/ip/".to_string()
}

fn key_prefix_ip_candidates(ip: Ipv4Addr) -> String {
    format!("/candidates/ip/{ip}/")
}

impl RouterActor {
    async fn read_all_ip_assignments(&mut self) -> Result<Vec<IpAssignment>> {
        let entries = self
            .doc
            .get_many(query_prefix(key_ip_assigned_prefix()))
            .await?
            .collect::<Vec<_>>()
            .await
            .iter()
            .filter_map(|entry| entry.as_ref().ok())
            .cloned()
            .collect::<Vec<_>>();

        let mut assignments = Vec::new();
        for entry in entries {
            if let Ok(assignment) = entry_to_value::<IpAssignment>(self.blobs.clone(), &entry).await
            {
                assignments.push(assignment);
            }
        }

        Ok(assignments)
    }

    async fn read_all_ip_candidates(&mut self) -> Result<Vec<IpCandidate>> {
        let entries = self
            .doc
            .get_many(query_prefix(key_ip_candidate_prefix()))
            .await?
            .collect::<Vec<_>>()
            .await
            .iter()
            .filter_map(|entry| entry.as_ref().ok())
            .cloned()
            .collect::<Vec<_>>();

        let mut candidates = Vec::new();
        for entry in entries {
            if let Ok(candidate) = entry_to_value::<IpCandidate>(self.blobs.clone(), &entry).await {
                candidates.push(candidate);
            }
        }

        Ok(candidates)
    }

    // Assigned IPs
    async fn read_ip_assignment(&mut self, ip: Ipv4Addr) -> Result<IpAssignment> {
        postcard::from_bytes::<IpAssignment>(
            &self
                .doc
                .get_one(query(key_ip_assigned(ip)))
                .await?
                .context("no assignment found")?
                .to_vec(),
        )
        .context("failed to deserialize endpoint_id")
    }

    // Candidate IPs
    async fn read_ip_candidates(&mut self, ip: Ipv4Addr) -> Result<Vec<IpCandidate>> {
        let entries = self
            .doc
            .get_many(query_prefix(key_prefix_ip_candidates(ip)))
            .await?
            .collect::<Vec<_>>()
            .await;
        debug!(
            "candidates for {ip}: {:?}",
            entries
                .iter()
                .map(|e| e.as_ref().ok().map(|e| e.content_hash()))
                .collect::<Vec<_>>()
        );
        let mut candidates = Vec::new();
        for entry in entries.into_iter().flatten() {
            if let Ok(b) = self.blobs.get_bytes(entry.content_hash()).await {
                if let Ok(candidate) = postcard::from_bytes::<IpCandidate>(&b) {
                    candidates.push(candidate);
                }
            }
        }

        Ok(candidates)
    }

    // write ip assigned
    async fn write_ip_assignment(&mut self, ip: Ipv4Addr, endpoint_id: EndpointId) -> Result<()> {
        if self.read_ip_assignment(ip).await.is_ok() {
            anyhow::bail!("ip already assigned");
        }

        // check for multiple candidates
        let mut candidates = self.read_ip_candidates(ip).await?;
        if candidates.is_empty() {
            bail!("no candidates for this ip");
        }

        candidates.sort_by_key(|c| {
            let mut hasher = sha2::Sha256::new();
            hasher.update(c.endpoint_id.as_bytes());
            hasher.update(ip.to_string().as_bytes());
            hasher.finalize()
        });

        if candidates[0].endpoint_id != endpoint_id {
            bail!("not the chosen candidate");
        }

        // We are the chosen one!
        // 1. write our ip assignment
        // 2. delete all candidates for this ip
        let data = postcard::to_stdvec(&IpAssignment { ip, endpoint_id })?;
        self.doc
            .set_bytes(self.author_id, key_ip_assigned(ip), data)
            .await?;

        let _ = self
            .doc
            .del(self.author_id, key_prefix_ip_candidates(ip))
            .await;

        Ok(())
    }

    // write ip candidate
    async fn write_ip_candidate(
        &mut self,
        ip: Ipv4Addr,
        endpoint_id: EndpointId,
    ) -> Result<IpCandidate> {
        // already assigned? don't write
        if self.read_ip_assignment(ip).await.is_ok() {
            anyhow::bail!("ip already assigned");
        }

        // read existing candidates; Ok(vec![]) when none exist
        let candidates = self.read_ip_candidates(ip).await.unwrap_or_default();

        // if someone else is already a candidate, treat as contested and skip writing
        if candidates.iter().any(|c| c.endpoint_id != endpoint_id) {
            anyhow::bail!("ip candidate already exists");
        }

        // idempotent for our own node_id
        let candidate = IpCandidate { ip, endpoint_id };
        let data = postcard::to_stdvec(&candidate)?;
        self.doc
            .set_bytes(self.author_id, key_ip_candidate(ip, endpoint_id), data)
            .await?;
        Ok(candidate)
    }

    async fn get_endpoint_id_from_ip(&mut self, ip: Ipv4Addr) -> Result<EndpointId> {
        self.read_all_ip_assignments()
            .await?
            .iter()
            .find(|assignment| assignment.ip == ip)
            .map(|a| a.endpoint_id)
            .ok_or_else(|| anyhow::anyhow!("no endpoint_id found for ip"))
    }

    async fn get_ip_from_endpoint_id(&mut self, endpoint_id: EndpointId) -> Result<Ipv4Addr> {
        self.read_all_ip_assignments()
            .await?
            .iter()
            .find(|assignment| assignment.endpoint_id == endpoint_id)
            .map(|a| a.ip)
            .ok_or_else(|| anyhow::anyhow!("no ip found for endpoint_id"))
    }
}

impl RouterActor {
    async fn advance_ip_state(&mut self) -> Result<bool> {
        match self.my_ip.clone() {
            RouterIp::NoIp => {
                let next_ip = self.get_next_ip().await?;

                self.my_ip = RouterIp::AquiringIp(
                    self.write_ip_candidate(next_ip, self.endpoint_id).await?,
                    tokio::time::Instant::now(),
                );
                Ok(false)
            }
            RouterIp::AquiringIp(ip_candidate, start_time) => {
                if start_time.elapsed() > Duration::from_secs(5) {
                    if self
                        .write_ip_assignment(ip_candidate.ip, ip_candidate.endpoint_id)
                        .await
                        .is_ok()
                    {
                        self.my_ip = RouterIp::AssignedIp(ip_candidate.ip);
                    } else {
                        self.my_ip = RouterIp::NoIp;
                    }
                }
                Ok(false)
            }
            _ => Ok(true),
        }
    }

    async fn get_next_ip(&mut self) -> Result<Ipv4Addr> {
        let all_assigned = self.read_all_ip_assignments().await?;
        let all_candidates = self.read_all_ip_candidates().await?;

        let all_ips = all_assigned
            .iter()
            .map(|assigned| assigned.ip)
            .chain(all_candidates.iter().map(|candidate| candidate.ip))
            .collect::<Vec<_>>();

        let highest_ip = if all_ips.is_empty() {
            Ipv4Addr::new(172, 22, 0, 2)
        } else {
            *all_ips
                .iter()
                .max_by_key(|&&ip| ip.octets()[2] as u16 * 256u16 + ip.octets()[3] as u16)
                .expect("no ips found")
        };

        let next_ip = Ipv4Addr::new(
            172,
            22,
            if highest_ip.octets()[3] == 255 { 1 } else { 0 } + highest_ip.octets()[2],
            if highest_ip.octets()[3] == 255 {
                0
            } else {
                highest_ip.octets()[3] + 1
            },
        );

        // to avoid overflow and therefore duplicates we check if this ip is already contained
        if all_ips.contains(&next_ip) {
            bail!("no more ips available");
        }
        Ok(next_ip)
    }
}
