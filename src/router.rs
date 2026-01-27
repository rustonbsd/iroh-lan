use std::{
    collections::{BTreeMap, HashSet},
    net::Ipv4Addr,
    time::Duration,
};

use ed25519_dalek::SigningKey;
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
use iroh_gossip::{
    api::{GossipReceiver, GossipSender},
    net::Gossip,
};
use iroh_topic_tracker::{TopicDiscoveryConfig, TopicDiscoveryExt, TopicDiscoveryHandle};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tracing::{debug, info, trace, warn};

use actor_helper::{Action, Actor, Handle, act, act_ok};

#[derive(Debug, Clone)]
pub struct Builder {
    entry_name: String,
    secret_key: SecretKey,
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
        let topic_discovery_config = TopicDiscoveryConfig::new(signing_key);
        let (gossip_sender, gossip_receiver, topic_handle) = loop {
            if let Ok((gossip_sender, gossip_receiver, topic_handle)) = gossip
            .subscribe_with_discovery_joined(
                topic_hash.to_vec(),
                vec![],
                endpoint.clone(),
                topic_discovery_config.clone(),
            )
            .await {
                break (gossip_sender, gossip_receiver, Some(topic_handle));
            } else {
                warn!("Failed to join topic; retrying in 2 second");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };

        let doc_peers = gossip_receiver
            .neighbors()
            .map(EndpointAddr::new)
            .collect::<Vec<_>>();

        debug!("[Doc peers]: {:?}", doc_peers);

        let author_id = docs.author_create().await?;
        let doc = docs
            .import(iroh_docs::DocTicket {
                capability: iroh_docs::Capability::Write(NamespaceSecret::from_bytes(&topic_hash)),
                nodes: doc_peers.clone(),
            })
            .await?;

        let wait_start = tokio::time::Instant::now();
        while match doc.get_sync_peers().await {
            Ok(Some(peers)) => peers.is_empty(),
            Ok(None) => true,
            Err(_) => true,
        } {
            if wait_start.elapsed() > Duration::from_secs(30) {
                warn!("Doc sync peers not available yet; continuing without peers");
                break;
            }
            debug!("Waiting for doc to be ready...");
            tokio::time::sleep(Duration::from_millis(500)).await;
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
                _topic: topic_handle,
                rx,
                blobs,
                my_ip: RouterIp::NoIp,
                assignments_cache: DocCache::new(),
                candidates_cache: DocCache::new(),
            };
            router_actor.run().await
        });

        Ok(Router { api })
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self {
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
    VerifyingIp(Ipv4Addr, tokio::time::Instant),
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
    pub(crate) _topic: Option<TopicDiscoveryHandle>,

    pub my_ip: RouterIp,

    assignments_cache: DocCache<IpAssignment>,
    candidates_cache: DocCache<IpCandidate>,
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

        let cleanup_interval_secs = 5 + rand::rng().random_range(0..10);
        let mut cleanup_tick = tokio::time::interval(Duration::from_secs(cleanup_interval_secs));
        cleanup_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("RouterActor started. EndpointId: {}", self.endpoint_id);

        let mut last_ip_state = self.my_ip.clone();
        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    trace!("Action received");
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl-C, stopping RouterActor");
                    break
                }

                // advance ip state machine
                _ = ip_tick.tick() => {
                    trace!("IP tick");
                    match self.advance_ip_state().await {
                        Ok(_) => if self.my_ip != last_ip_state {
                            info!("Router IP state changed: {:?} -> {:?}", last_ip_state, self.my_ip);
                            last_ip_state = self.my_ip.clone();
                        },
                        Err(e) => {
                            warn!("Error advancing IP state: {}", e);
                        }
                    }
                }

                _ = cleanup_tick.tick() => {
                    trace!("Cleanup tick");
                    if let Err(e) = self.perform_cleanup().await {
                        warn!("Error performing cleanup: {}", e);
                    }
                }

            }
        }

        Ok(())
    }
}

fn current_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpAssignment {
    pub ip: Ipv4Addr,
    pub endpoint_id: EndpointId,
    pub last_updated: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpCandidate {
    pub ip: Ipv4Addr,
    pub endpoint_id: EndpointId,
    pub last_updated: u64,
}

async fn entry_to_value<S: for<'a> Deserialize<'a>>(
    blobs: MemStore,
    entry: &Entry,
) -> anyhow::Result<S> {
    let b = blobs.get_bytes(entry.content_hash()).await?;
    postcard::from_bytes::<S>(&b).context("failed to deserialize value")
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

#[derive(Debug, Clone)]
struct DocCache<T> {
    value: Vec<T>,
    last_read: Option<tokio::time::Instant>,
}

impl<T> DocCache<T> {
    fn new() -> Self {
        Self {
            value: Vec::new(),
            last_read: None,
        }
    }

    fn get(&self, ttl: Duration) -> Option<&[T]> {
        match self.last_read {
            Some(ts) if ts.elapsed() < ttl => Some(&self.value),
            _ => None,
        }
    }

    fn set(&mut self, value: Vec<T>) {
        self.value = value;
        self.last_read = Some(tokio::time::Instant::now());
    }

    fn invalidate(&mut self) {
        self.last_read = None;
    }
}

impl RouterActor {
    const DOC_READ_MIN_INTERVAL: Duration = Duration::from_secs(2);
    const ASSIGNMENT_STALE_SECS: u64 = 300;
    const CANDIDATE_STALE_SECS: u64 = 60;


    async fn read_all_ip_assignments(&mut self) -> Result<Vec<IpAssignment>> {
        trace!("Reading all IP assignments");
        if let Some(cached) = self.assignments_cache.get(Self::DOC_READ_MIN_INTERVAL) {
            return Ok(cached.to_vec());
        }
        let entries = self
            .doc
            .get_many(query_prefix(key_ip_assigned_prefix()))
            .await?
            .collect::<Vec<_>>()
            .await
            .iter()
            .filter_map(|entry| { if let Ok(entry) = entry.as_ref() {  Some(entry) } else { warn!("[PEER] entry is not ok!"); None } })
            .cloned()
            .collect::<Vec<_>>();

        let mut assignments = Vec::new();
        for entry in entries {
            if let Ok(assignment) = entry_to_value::<IpAssignment>(self.blobs.clone(), &entry).await
            {
                assignments.push(assignment);
            }
        }
        trace!("Found {} IP assignments", assignments.len());
        self.assignments_cache.set(assignments.clone());
        Ok(assignments)
    }

    async fn read_all_ip_candidates(&mut self) -> Result<Vec<IpCandidate>> {
        trace!("Reading all IP candidates");
        if let Some(cached) = self.candidates_cache.get(Self::DOC_READ_MIN_INTERVAL) {
            return Ok(cached.to_vec());
        }
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
        trace!("Found {} IP candidates", candidates.len());
        self.candidates_cache.set(candidates.clone());
        Ok(candidates)
    }


    async fn clear_ip_candidate(&mut self, ip: Ipv4Addr, endpoint_id: EndpointId) -> Result<()> {
        debug!("Clearing IP candidate {} for {}", ip, endpoint_id);
        self.doc
            .del(self.author_id, key_ip_candidate(ip, endpoint_id))
            .await?;
        self.candidates_cache.invalidate();
        Ok(())
    }

    // Assigned IPs
    async fn read_ip_assignment(&mut self, ip: Ipv4Addr) -> Result<IpAssignment> {
        self.read_all_ip_assignments()
            .await?
            .into_iter()
            .find(|assignment| assignment.ip == ip)
            .context("no assignment found")
    }

    // Candidate IPs
    async fn read_ip_candidates(&mut self, ip: Ipv4Addr) -> Result<Vec<IpCandidate>> {
        let candidates = self.read_all_ip_candidates().await?;
        let now = current_time();
        let filtered = candidates
            .into_iter()
            .filter(|candidate| candidate.ip == ip)
            .filter(|candidate| now.saturating_sub(candidate.last_updated) <= Self::CANDIDATE_STALE_SECS)
            .collect::<Vec<_>>();
        debug!("candidates for {ip}: {}", filtered.len());
        Ok(filtered)
    }

    // write ip assigned
    async fn write_ip_assignment(&mut self, ip: Ipv4Addr, endpoint_id: EndpointId) -> Result<()> {
        info!("Attempting to assign IP {} to {}", ip, endpoint_id);

        let existing_assignment = self.read_ip_assignment(ip).await.ok();

        if let Some(existing) = &existing_assignment {
            if existing.endpoint_id != endpoint_id {
                anyhow::bail!("ip already assigned to another node");
            }
        } else {
            // New assignment. check for multiple candidates
            let candidates = self.read_ip_candidates(ip).await?;
            if candidates.is_empty() {
                bail!("no candidates for this ip");
            }

            if candidates
                .iter()
                .any(|c| c.endpoint_id != self.endpoint_id && self.endpoint_id < c.endpoint_id)
            {
                self.clear_ip_candidate(ip, endpoint_id).await?;
                bail!("another candidate with higher endpoint_id exists");
            }
        }

        // We are the chosen one!
        // 1. write our ip assignment
        // 2. delete all candidates for this ip
        debug!("Winning candidate for IP {}. Writing assignment.", ip);
        let data = postcard::to_stdvec(&IpAssignment {
            ip,
            endpoint_id,
            last_updated: current_time(),
        })?;
        self.doc
            .set_bytes(self.author_id, key_ip_assigned(ip), data)
            .await?;

        self.assignments_cache.invalidate();

        self.clear_ip_candidate(ip, endpoint_id).await.ok();

        self.candidates_cache.invalidate();

        Ok(())
    }

    // write ip candidate
    async fn write_ip_candidate(
        &mut self,
        ip: Ipv4Addr,
        endpoint_id: EndpointId,
    ) -> Result<IpCandidate> {
        debug!("Writing IP candidate {} for {}", ip, endpoint_id);
        // already assigned? don't write
        if self.read_ip_assignment(ip).await.is_ok() {
            anyhow::bail!("ip already assigned");
        }

        // read existing candidates; Ok(vec![]) when none exist
        let candidates = self.read_ip_candidates(ip).await.unwrap_or_default();

        // if someone else is already a candidate, treat as contested and skip writing
        if candidates.iter().any(|c| c.endpoint_id != endpoint_id) {
            warn!("Contested IP candidate {}. Another candidate exists.", ip);
            anyhow::bail!("ip candidate already exists");
        }

        // idempotent for our own node_id
        let candidate = IpCandidate {
            ip,
            endpoint_id,
            last_updated: current_time(),
        };
        let data = postcard::to_stdvec(&candidate)?;
        self.doc
            .set_bytes(self.author_id, key_ip_candidate(ip, endpoint_id), data)
            .await?;
        self.candidates_cache.invalidate();
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
        trace!("Advancing IP state. Current: {:?}", self.my_ip);
        match self.my_ip.clone() {
            RouterIp::NoIp => {
                let next_ip = self.get_next_ip().await?;
                info!("Trying to acquire IP candidate: {}", next_ip);

                self.my_ip = RouterIp::AquiringIp(
                    self.write_ip_candidate(next_ip, self.endpoint_id).await?,
                    tokio::time::Instant::now(),
                );
                Ok(false)
            }
            RouterIp::AquiringIp(ip_candidate, start_time) => {
                let elapsed = start_time.elapsed();

                if let Ok(candidates) = self.read_ip_candidates(ip_candidate.ip).await
                    && candidates.iter().any(|c| {
                        c.endpoint_id != self.endpoint_id && self.endpoint_id < c.endpoint_id
                    })
                {
                    warn!(
                        "Conflict detected for {:?} during acquisition wait. Aborting.",
                        ip_candidate.ip
                    );
                    self.clear_ip_candidate(ip_candidate.ip, ip_candidate.endpoint_id)
                        .await?;
                    self.my_ip = RouterIp::NoIp;
                    return Ok(false);
                }

                // Wait at least 5 seconds
                if elapsed > Duration::from_secs(5) {
                    // Jitter: 20% chance to proceed per tick (approx 500ms)
                    if rand::rng().random_bool(0.2) {
                        info!(
                            "Attempting to finalize IP assignment for {}",
                            ip_candidate.ip
                        );
                        if self
                            .write_ip_assignment(ip_candidate.ip, ip_candidate.endpoint_id)
                            .await
                            .is_ok()
                        {
                            info!(
                                "Written assignment for IP: {}. Verifying...",
                                ip_candidate.ip
                            );
                            self.my_ip =
                                RouterIp::VerifyingIp(ip_candidate.ip, tokio::time::Instant::now());
                        } else {
                            warn!(
                                "Failed to write assignment for {}. Restarting negotiation.",
                                ip_candidate.ip
                            );
                            self.clear_ip_candidate(ip_candidate.ip, ip_candidate.endpoint_id)
                                .await
                                .ok();
                            self.my_ip = RouterIp::NoIp;
                        }
                    }
                }
                Ok(false)
            }
            RouterIp::VerifyingIp(ip, start_time) => {
                // Wait 2 seconds to ensure propagation
                if start_time.elapsed() > Duration::from_secs(2) {
                    trace!("Verifying IP assignment logic for {}", ip);
                    let assignment = self.read_ip_assignment(ip).await;
                    match assignment {
                        Ok(a) if a.endpoint_id == self.endpoint_id => {
                            info!("Verified ownership of IP: {}", ip);
                            self.my_ip = RouterIp::AssignedIp(ip);
                        }
                        _ => {
                            warn!(
                                "Verification failed for IP: {}. Lost to another node or ghost.",
                                ip
                            );
                            self.my_ip = RouterIp::NoIp;
                        }
                    }
                }
                Ok(false)
            }
            RouterIp::AssignedIp(my_ip) => {
                match self.read_ip_assignment(my_ip).await {
                    Ok(ip_assignment) => {
                        if ip_assignment.endpoint_id != self.endpoint_id {
                            self.my_ip = RouterIp::NoIp;
                            warn!(
                                "Lost IP assignment for {}. Expected owner: {}, Found: {}. Restarting negotiation.",
                                my_ip, self.endpoint_id, ip_assignment.endpoint_id
                            );
                            Ok(false)
                        } else {
                            // Refresh if needed (every 30s)
                            if current_time() - ip_assignment.last_updated > 30 {
                                info!("Refreshing IP assignment for {}", my_ip);
                                if let Err(e) =
                                    self.write_ip_assignment(my_ip, self.endpoint_id).await
                                {
                                    warn!("Failed to refresh IP assignment: {}", e);
                                    // Don't lose IP immediately, retry next tick
                                }
                            }
                            Ok(true)
                        }
                    }
                    Err(_) => {
                        self.my_ip = RouterIp::NoIp;
                        warn!("Assignment lost/deleted for {}. Restarting.", my_ip);
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn get_next_ip(&mut self) -> Result<Ipv4Addr> {
        let all_assigned = self.read_all_ip_assignments().await?;
        let all_candidates = self.read_all_ip_candidates().await?;

        let mut used_ips = all_assigned
            .iter()
            .map(|ip_assignment| ip_assignment.ip)
            .collect::<HashSet<_>>();
        used_ips.extend(
            all_candidates
                .iter()
                .map(|ip_candidate| ip_candidate.ip)
                .collect::<HashSet<_>>(),
        );

        // Scan for the first available IP starting from 172.22.0.2
        for i in 2u16..65534 {
            let octet3 = (i >> 8) as u8;
            let octet4 = (i & 0xFF) as u8;

            // Skip network/broadcast addresses if needed, though 172.22.0.0/16 is a large subnet.
            // Keeping it simple: avoid .0 and .255 in the last octet to be safe for standard /24 clients
            if octet4 == 0 || octet4 == 255 {
                continue;
            }

            let ip = Ipv4Addr::new(172, 22, octet3, octet4);
            if !used_ips.contains(&ip) {
                return Ok(ip);
            }
        }

        bail!("no more ips available");
    }

    async fn perform_cleanup(&mut self) -> Result<()> {
        let now = current_time();

        // Cleanup assignments
        for a in self.read_all_ip_assignments().await? {
            if now.saturating_sub(a.last_updated) > Self::ASSIGNMENT_STALE_SECS {
                info!(
                    "Removing stale IP assignment for {} (last updated {}s ago)",
                    a.ip,
                    now - a.last_updated
                );
                if let Err(e) = self.doc.del(self.author_id, key_ip_assigned(a.ip)).await {
                    warn!("Failed to delete stale IP {}: {}", a.ip, e);
                } else {
                    info!("Cleaned up assignment: {:?}", a);
                    self.assignments_cache.invalidate();
                }
            }
        }

        // Cleanup candidates
        for c in self.read_all_ip_candidates().await? {
            if now.saturating_sub(c.last_updated) > Self::CANDIDATE_STALE_SECS {
                info!(
                    "Removing stale IP candidate for {} (last updated {}s ago)",
                    c.ip,
                    now - c.last_updated
                );
                self.clear_ip_candidate(c.ip, c.endpoint_id).await.ok();
                self.candidates_cache.invalidate();
            }
        }
        Ok(())
    }
}
