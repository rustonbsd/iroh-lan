use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    time::Duration,
};

use ed25519_dalek::SigningKey;
use iroh_gossip::net::Gossip;
use iroh_topic_tracker::{
    TopicDiscoveryConfig, TopicDiscoveryExt, TopicDiscoveryHandle, TopicDiscoveryHook,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tracing::{debug, info, trace, warn};

use anyhow::{Result, bail};
use iroh::{Endpoint, EndpointId, SecretKey};

use actor_helper::{Action, Handle, Receiver, act, act_ok};

use crate::kv::{Kv, KvEvent};

const CANDIDATE_PHASE_DURATION: Duration = Duration::from_secs(10);
const VERIFY_IP_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Builder {
    topic_discovery_hook: TopicDiscoveryHook,
    entry_name: String,
    secret_key: SecretKey,
    password: String,
    endpoint: Option<Endpoint>,
    gossip: Option<Gossip>,
}

impl Builder {
    pub fn new(topic_discovery_hook: TopicDiscoveryHook) -> Builder {
        Builder {
            topic_discovery_hook,
            entry_name: String::default(),
            secret_key: SecretKey::generate(),
            password: String::default(),
            endpoint: None,
            gossip: None,
        }
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

        let topic_initials = format!("lanparty-{}", self.entry_name);
        let secret_initials = format!("{topic_initials}-{}-secret", self.password)
            .as_bytes()
            .to_vec();

        let mut topic_hasher = sha2::Sha512::new();
        topic_hasher.update("iroh-lan-topic");
        topic_hasher.update(&secret_initials);
        let topic_hash: [u8; 32] = topic_hasher.finalize()[..32].try_into()?;

        let signing_key = SigningKey::from_bytes(&self.secret_key.to_bytes());
        let topic_discovery_config =
            TopicDiscoveryConfig::builder(signing_key, self.topic_discovery_hook.clone())
                .connection_timeout(Duration::from_secs(30))
                .announce_interval(Duration::from_secs(15 * 60))
                .first_connected_duration(Some(Duration::from_secs(60)))
                .discovery_interval_first_connected(Duration::from_secs(4))
                .dht_retries(None)
                .build();
        let (gossip_sender, gossip_receiver, topic_handle) = loop {
            if let Ok((gossip_sender, gossip_receiver, topic_handle)) = gossip
                .subscribe_with_discovery_joined(
                    topic_hash.to_vec(),
                    vec![],
                    topic_discovery_config.clone(),
                )
                .await
            {
                break (gossip_sender, gossip_receiver, Some(topic_handle));
            } else {
                warn!("Failed to join topic; retrying in 2 second");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        };

        info!("Joined topic with hash: {:x?}", topic_hash);

        let kv = Kv::spawn(endpoint.id(), gossip_sender, gossip_receiver);

        let (api, _) = Handle::spawn_with(
            RouterActor {
                kv,
                _endpoint: endpoint.clone(),
                endpoint_id: endpoint.id(),
                topic: topic_handle,
                my_ip: RouterIp::NoIp,
                assignments: BTreeMap::new(),
                candidates: BTreeMap::new(),
                startup_entries: BTreeSet::new(),
            },
            |mut actor, rx| async move { actor.run(rx).await },
        );
        Ok(Router { api })
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
    pub kv: Kv,

    pub(crate) _endpoint: Endpoint,
    pub endpoint_id: EndpointId,
    pub(crate) topic: Option<TopicDiscoveryHandle>,

    pub my_ip: RouterIp,

    assignments: BTreeMap<Ipv4Addr, IpAssignment>,
    candidates: BTreeMap<Ipv4Addr, BTreeMap<EndpointId, IpCandidate>>,
    startup_entries: BTreeSet<EndpointId>,
}

impl Router {
    pub fn builder(topic_discovery_hook: TopicDiscoveryHook) -> Builder {
        Builder::new(topic_discovery_hook)
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

                let assignments = actor.read_all_ip_assignments(true)?;
                for a in assignments {
                    map.insert(a.endpoint_id, Some(a.ip));
                }

                let cands = actor.read_all_ip_candidates(true)?;
                for c in cands {
                    map.entry(c.endpoint_id).or_insert(None);
                }

                map.remove(&actor.endpoint_id);

                Ok(map.into_iter().collect())
            }))
            .await
    }
}

impl RouterActor {
    async fn run(&mut self, rx: Receiver<Action<RouterActor>>) -> Result<()> {
        let mut ip_tick = tokio::time::interval(Duration::from_millis(1000));
        ip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!("RouterActor started. EndpointId: {}", self.endpoint_id);

        let mut last_ip_state = self.my_ip.clone();

        let mut kv_sub = self.kv.subscribe();
        self.populate_from_kv().await;

        loop {
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                    action(self).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl-C, stopping RouterActor");
                    break
                }

                result = kv_sub.recv() => {
                    match result {
                        Ok(event) => {
                            self.handle_kv_event(&event);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("[ROUTER_LOOP] kv_sub lagged by {} messages!", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!("[ROUTER_LOOP] kv_sub channel closed!");
                            break;
                        }
                    }
                }

                // advance ip state machine
                _ = ip_tick.tick() => {
                    if self.startup_entries.len() < 2 {

                        // Write our startup entry (immediately broadcast via gossip).
                        let startup_key = format!("{}{}", key_startup_prefix(), self.endpoint_id);
                        trace!("[ROUTER_LOOP] Calling kv.insert for startup");
                        self.kv.insert(&startup_key).await;
                        trace!("[ROUTER_LOOP] kv.insert completed");
                        trace!("Not advancing IP state, waiting for startup entries. Current count: {}", self.startup_entries.len());
                        continue;
                    }
                    match self.advance_ip_state().await {
                        Ok(_) => {
                            if self.my_ip != last_ip_state {
                                info!("Router IP state changed: {:?} -> {:?}", last_ip_state, self.my_ip);
                                last_ip_state = self.my_ip.clone();
                            }
                        },
                        Err(e) => {
                            warn!("Error advancing IP state: {}", e);
                        }
                    }
                }
            }
        }
        warn!("RouterActor stopped.");
        Ok(())
    }
}

impl RouterActor {
    async fn populate_from_kv(&mut self) {
        let all_entries = self.kv.query_all_entries().await;
        for (key, timestamp) in all_entries {
            self.apply_key(&key, timestamp);
        }
        debug!(
            "Populated from Kv: {} startup, {} assignments, {} candidate IPs",
            self.startup_entries.len(),
            self.assignments.len(),
            self.candidates.len(),
        );
    }

    fn handle_kv_event(&mut self, event: &KvEvent) {
        self.apply_key(&event.key, event.timestamp);
    }

    fn apply_key(&mut self, key: &str, timestamp: u64) {
        let startup_prefix = key_startup_prefix();
        let assigned_prefix = key_assigned_ip();
        let candidate_prefix = key_candidate_ip_prefix();

        if key.starts_with(&startup_prefix) {
            if let Ok(endpoint_id) = decode_endpoint_id_from_startup(key)
                && self.startup_entries.insert(endpoint_id)
            {
                info!(
                    "Startup entry added for {}. Total: {}",
                    endpoint_id,
                    self.startup_entries.len()
                );
            }
        } else if key.starts_with(&assigned_prefix) {
            if let Ok(ip_assignment) = decode_ip_assignment(key, timestamp) {
                self.assignments.insert(ip_assignment.ip, ip_assignment);
            }
        } else if key.starts_with(&candidate_prefix)
            && let Ok(ip_candidate) = decode_ip_candidate(key, timestamp)
        {
            self.candidates
                .entry(ip_candidate.ip)
                .or_default()
                .insert(ip_candidate.endpoint_id, ip_candidate);
        }
    }
}

fn current_time() -> u64 {
    chrono::Utc::now().timestamp() as u64
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

impl IpAssignment {
    pub fn is_stale(&self) -> bool {
        is_stale(self.last_updated, RouterActor::ASSIGNMENT_STALE_SECS)
    }
}

impl IpCandidate {
    pub fn is_stale(&self) -> bool {
        is_stale(self.last_updated, RouterActor::CANDIDATE_STALE_SECS)
    }
}

fn is_stale(last_updated: u64, stale_secs: u64) -> bool {
    current_time().saturating_sub(last_updated) > stale_secs
}

fn decode_ip_assignment(key: &str, timestamp: u64) -> Result<IpAssignment> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() != 5 {
        warn!(
            "Invalid key format for IpAssignment (invalid length): {}",
            key
        );
        anyhow::bail!("Invalid key format for IpAssignment");
    }
    if let (Ok(ip), Ok(endpoint_id)) =
        (parts[3].parse::<Ipv4Addr>(), parts[4].parse::<EndpointId>())
    {
        Ok(IpAssignment {
            ip,
            endpoint_id,
            last_updated: timestamp,
        })
    } else {
        warn!(
            "Failed to parse IpAssignment from key (parse failed): {}",
            key
        );
        anyhow::bail!("Invalid key format for IpAssignment");
    }
}

fn encode_ip_assignment(assignment: &IpAssignment) -> String {
    format!("/assigned/ip/{}/{}", assignment.ip, assignment.endpoint_id)
}

fn key_assigned_ip() -> String {
    "/assigned/ip/".to_string()
}

fn decode_ip_candidate(key: &str, timestamp: u64) -> Result<IpCandidate> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() != 5 {
        warn!(
            "Invalid key format for IpCandidate (invalid length): {}",
            key
        );
        anyhow::bail!("Invalid key format for IpCandidate");
    }
    if let (Ok(ip), Ok(endpoint_id)) =
        (parts[3].parse::<Ipv4Addr>(), parts[4].parse::<EndpointId>())
    {
        Ok(IpCandidate {
            ip,
            endpoint_id,
            last_updated: timestamp,
        })
    } else {
        warn!(
            "Failed to parse IpCandidate from key (parse failed): {}",
            key
        );
        anyhow::bail!("Invalid key format for IpCandidate");
    }
}

fn encode_ip_candidate(candidate: &IpCandidate) -> String {
    format!("/candidates/ip/{}/{}", candidate.ip, candidate.endpoint_id)
}

fn decode_endpoint_id_from_startup(key: &str) -> Result<EndpointId> {
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() != 3 {
        warn!(
            "Invalid key format for startup entry (invalid length): {}",
            key
        );
        anyhow::bail!("Invalid key format for startup entry");
    }
    if let Ok(endpoint_id) = parts[2].parse::<EndpointId>() {
        Ok(endpoint_id)
    } else {
        warn!(
            "Failed to parse EndpointId from startup key (parse failed): {}",
            key
        );
        anyhow::bail!("Invalid key format for startup entry");
    }
}

fn key_candidate_ip_prefix() -> String {
    "/candidates/ip/".to_string()
}

fn key_startup_prefix() -> String {
    "/startup/".to_string()
}

impl RouterActor {
    const ASSIGNMENT_STALE_SECS: u64 = 300;
    const CANDIDATE_STALE_SECS: u64 = 60;
    const IP_RANGES: [(usize, Ipv4Addr); u16::MAX as usize - 2] = {
        let mut data = [(0, Ipv4Addr::UNSPECIFIED); u16::MAX as usize - 2];
        let mut order = 2usize;
        while order < u16::MAX as usize {
            let octet3 = (order >> 8) as u8;
            let octet4 = (order & 0xFF) as u8;
            data[order - 2] = (order, Ipv4Addr::new(172, 30, octet3, octet4));
            order += 1;
        }
        data
    };

    fn is_initialized(&self) -> bool {
        self.startup_entries.len() >= 2
    }

    fn read_all_ip_assignments(&self, filter_stale: bool) -> Result<Vec<IpAssignment>> {
        if !self.is_initialized() {
            warn!(
                "[Kv] read_all_ip_assignments called before initialized. Startup entries count: {}",
                self.startup_entries.len()
            );
            bail!("[Kv] not initialized yet");
        }
        Ok(self
            .assignments
            .values()
            .filter(|assignment| !filter_stale || !assignment.is_stale())
            .cloned()
            .collect::<Vec<_>>())
    }

    fn read_all_ip_candidates(&self, filter_stale: bool) -> Result<Vec<IpCandidate>> {
        if !self.is_initialized() {
            warn!(
                "[Kv] read_all_ip_candidates called before initialized. Startup entries count: {}",
                self.startup_entries.len()
            );
            bail!("[Kv] not initialized yet");
        }
        Ok(self
            .candidates
            .values()
            .flatten()
            .filter(|(_, candidate)| !filter_stale || !candidate.is_stale())
            .map(|(_, candidate)| candidate.clone())
            .collect::<Vec<_>>())
    }

    // Assigned IPs
    fn read_ip_assignment(&self, ip: Ipv4Addr, filter_stale: bool) -> Result<Option<IpAssignment>> {
        if !self.is_initialized() {
            warn!(
                "[Kv] read_ip_assignment called before initialized. Startup entries count: {}",
                self.startup_entries.len()
            );
            bail!("[Kv] not initialized yet");
        }
        let mut assignments = self
            .assignments
            .iter()
            .filter(|(_, assignment)| !filter_stale || !assignment.is_stale())
            .map(|(_, assignment)| assignment)
            .collect::<Vec<_>>();
        assignments.sort_by_key(|a| a.last_updated);
        Ok(assignments.into_iter().rev().find(|a| a.ip == ip).cloned())
    }

    // Candidate IPs
    fn read_ip_candidates(&self, ip: Ipv4Addr, filter_stale: bool) -> Result<Vec<IpCandidate>> {
        if !self.is_initialized() {
            warn!(
                "[Kv] read_ip_candidates called before initialized. Startup entries count: {}",
                self.startup_entries.len()
            );
            bail!("[Kv] not initialized yet");
        }
        let mut ip_candidates = match self.candidates.get(&ip) {
            Some(candidates) => candidates
                .values()
                .filter(|candidate| !filter_stale || !candidate.is_stale())
                .cloned()
                .collect::<Vec<_>>(),
            None => vec![],
        };
        ip_candidates.sort_by_key(|candidate| candidate.last_updated);
        debug!("candidates for {ip}: {}", ip_candidates.len());
        Ok(ip_candidates)
    }

    // write ip assigned
    async fn write_ip_assignment(
        &mut self,
        ip: Ipv4Addr,
        endpoint_id: EndpointId,
        force_override: bool,
    ) -> Result<()> {
        if !self.is_initialized() {
            warn!(
                "[Kv] write_ip_assignment called before initialized. Startup entries count: {}",
                self.startup_entries.len()
            );
            bail!("[Kv] not initialized yet");
        }
        info!("Attempting to assign IP {} to {}", ip, endpoint_id);

        let existing_assignment = self.read_ip_assignment(ip, true)?;

        if let Some(existing) = &existing_assignment {
            if existing.endpoint_id != endpoint_id && !force_override {
                anyhow::bail!("ip already assigned to another node");
            }
        } else {
            // New assignment. check for multiple candidates
            let candidates = self.read_ip_candidates(ip, true)?;
            if candidates
                .iter()
                .filter(|c| if c.endpoint_id != self.endpoint_id && self.endpoint_id < c.endpoint_id {
                    debug!(
                        "Existing candidate {} has higher priority than us for IP {}. Not writing new candidate.",
                        c.endpoint_id, ip
                    );
                    true
                } else {
                    false
                })
                .count()
                > 0
                || !candidates.iter().any(|c| c.endpoint_id == self.endpoint_id)
            {
                bail!("no candidates for this ip");
            }
        }

        // We are the chosen one!
        debug!("Winning candidate for IP {}. Writing assignment.", ip);
        let now = current_time();
        let ip_assignment = IpAssignment {
            ip,
            endpoint_id,
            last_updated: now,
        };
        self.kv.insert(encode_ip_assignment(&ip_assignment)).await;
        debug!("Wrote IP assignment {} for {}.", ip, endpoint_id);

        Ok(())
    }

    // write ip candidate
    async fn write_ip_candidate(
        &mut self,
        ip: Ipv4Addr,
        endpoint_id: EndpointId,
    ) -> Result<IpCandidate> {
        if !self.is_initialized() {
            warn!(
                "[Kv] write_ip_candidate called before initialized. Startup entries count: {}",
                self.startup_entries.len()
            );
            bail!("[Kv] not initialized yet");
        }
        // already assigned? don't write
        if let Some(assignment) = self.read_ip_assignment(ip, true)?
            && assignment.endpoint_id != endpoint_id
            && self.endpoint_id < assignment.endpoint_id
        {
            anyhow::bail!("ip assignment already assigned");
        }

        if let Ok(candidates) = self.read_ip_candidates(ip, true)
            && candidates
                .iter()
                .filter(|c| if c.endpoint_id != self.endpoint_id && self.endpoint_id < c.endpoint_id {
                    debug!(
                        "Existing candidate {} has higher priority than us for IP {}. Not writing new candidate.",
                        c.endpoint_id, ip
                    );
                    true
                } else {
                    false
                })
                .count()
                > 0
        {
            anyhow::bail!("ip candidate already exists");
        }

        // idempotent for our own node_id
        let now = current_time();
        let candidate = IpCandidate {
            ip,
            endpoint_id,
            last_updated: now,
        };
        self.kv.insert(encode_ip_candidate(&candidate)).await;
        debug!("Wrote IP candidate {} for {}.", ip, endpoint_id);

        Ok(candidate)
    }

    async fn get_endpoint_id_from_ip(&mut self, ip: Ipv4Addr) -> Result<EndpointId> {
        self.read_ip_assignment(ip, true)?
            .map(|a| a.endpoint_id)
            .ok_or_else(|| anyhow::anyhow!("no endpoint_id found for ip"))
    }

    async fn get_ip_from_endpoint_id(&mut self, endpoint_id: EndpointId) -> Result<Ipv4Addr> {
        self.read_all_ip_assignments(true)?
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

                if self.read_ip_assignment(next_ip, true)?.is_some() {
                    debug!(
                        "IP {} already assigned to another node. Skipping candidate write.",
                        next_ip
                    );
                    return Ok(false);
                }

                self.my_ip = RouterIp::AquiringIp(
                    self.write_ip_candidate(next_ip, self.endpoint_id).await?,
                    tokio::time::Instant::now(),
                );
                Ok(false)
            }
            RouterIp::AquiringIp(ip_candidate, start_time) => {
                let elapsed = start_time.elapsed();

                debug!(
                    "RouterIp::AquiringIp: {:?}",
                    self.read_ip_candidates(ip_candidate.ip, true)?
                );
                if let Ok(candidates) = self.read_ip_candidates(ip_candidate.ip, true)
                    && candidates.iter().any(|c| {
                        c.endpoint_id != self.endpoint_id
                            && (self.endpoint_id < c.endpoint_id
                                || (self.endpoint_id >= c.endpoint_id
                                    && current_time().saturating_sub(c.last_updated)
                                        > CANDIDATE_PHASE_DURATION.as_secs()))
                    })
                {
                    warn!(
                        "Conflict detected for {:?} during acquisition wait. Aborting.",
                        ip_candidate.ip
                    );
                    self.my_ip = RouterIp::NoIp;
                    return Ok(false);
                }

                if let Ok(Some(assignment)) = self.read_ip_assignment(ip_candidate.ip, true)
                    && assignment.endpoint_id != self.endpoint_id
                    && (self.endpoint_id < assignment.endpoint_id
                        || (self.endpoint_id >= assignment.endpoint_id
                            && current_time().saturating_sub(assignment.last_updated)
                                > VERIFY_IP_DURATION.as_secs()))
                {
                    warn!(
                        "IP {} got assigned to {} during acquisition wait. Aborting.",
                        ip_candidate.ip, assignment.endpoint_id
                    );
                    self.my_ip = RouterIp::NoIp;
                    return Ok(false);
                }

                self.write_ip_candidate(ip_candidate.ip, self.endpoint_id)
                    .await?;

                // Wait at least 5 seconds
                if elapsed > CANDIDATE_PHASE_DURATION {
                    // Jitter: 20% chance to proceed per tick (approx 500ms)
                    if rand::rng().random_bool(0.2) {
                        // if we are connected to peers that we don't yet see in any of the candidate records,
                        // that leaves the risk of a potential collision, so we wait until we see all currently connected peers in at least one of the candidates
                        if let Some(peers) =
                            self.topic.as_ref().map(|topic| topic.get_connected_peers())
                        {
                            let all_candidates = self.read_all_ip_candidates(true)?;
                            if peers
                                .iter()
                                .all(|p| all_candidates.iter().any(|c| c.endpoint_id == *p))
                            {
                                debug!(
                                    "All connected peers are visible in candidates for IP {}. Proceeding with acquisition.",
                                    ip_candidate.ip
                                );
                            } else {
                                warn!(
                                    "Not all connected peers are visible in candidates for IP {}. Waiting longer to avoid potential collision.",
                                    ip_candidate.ip
                                );
                                return Ok(false);
                            }
                        } else {
                            return Ok(false);
                        }

                        info!(
                            "Attempting to finalize IP assignment for {}",
                            ip_candidate.ip
                        );
                        if self
                            .write_ip_assignment(ip_candidate.ip, ip_candidate.endpoint_id, false)
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
                            self.my_ip = RouterIp::NoIp;
                        }
                    }
                }
                Ok(false)
            }
            RouterIp::VerifyingIp(ip, start_time) => {
                // Wait 2 seconds to ensure propagation
                if start_time.elapsed() > VERIFY_IP_DURATION {
                    trace!("Verifying IP assignment logic for {}", ip);
                    let assignment = self.read_ip_assignment(ip, true)?;
                    match assignment {
                        Some(a) if a.endpoint_id == self.endpoint_id => {
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
                match self.read_ip_assignment(my_ip, true)? {
                    Some(ip_assignment) => {
                        if ip_assignment.endpoint_id != self.endpoint_id {
                            if ip_assignment.endpoint_id < self.endpoint_id {
                                if current_time().saturating_sub(ip_assignment.last_updated) > 30 {
                                    info!("Refreshing IP assignment for {}", my_ip);
                                    if let Err(e) = self
                                        .write_ip_assignment(my_ip, self.endpoint_id, true)
                                        .await
                                    {
                                        warn!("Failed to refresh IP assignment: {}", e);
                                        // Don't lose IP immediately, retry next tick
                                    }
                                }
                                Ok(true)
                            } else {
                                self.my_ip = RouterIp::NoIp;
                                warn!(
                                    "Lost IP assignment for {}. Expected owner: {}, Found: {}. Restarting negotiation.",
                                    my_ip, self.endpoint_id, ip_assignment.endpoint_id
                                );
                                Ok(false)
                            }
                        } else {
                            // Refresh if needed (every 30s)
                            if current_time().saturating_sub(ip_assignment.last_updated) > 30 {
                                info!("Refreshing IP assignment for {}", my_ip);
                                if let Err(e) = self
                                    .write_ip_assignment(my_ip, self.endpoint_id, false)
                                    .await
                                {
                                    warn!("Failed to refresh IP assignment: {}", e);
                                    // Don't lose IP immediately, retry next tick
                                }
                            }
                            Ok(true)
                        }
                    }
                    None => {
                        self.my_ip = RouterIp::NoIp;
                        warn!("Assignment lost/deleted for {}. Restarting.", my_ip);
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn get_next_ip(&mut self) -> Result<Ipv4Addr> {
        let mut next_ip_range = Self::IP_RANGES.to_vec();

        for assignment in self.read_all_ip_assignments(false)? {
            let index = ((u16::from(assignment.ip.octets()[2]) << 8)
                | u16::from(assignment.ip.octets()[3])) as usize
                - 2;
            next_ip_range[index].0 += u16::MAX as usize; // push used IPs to the end of the list
        }

        for candidate in self.read_all_ip_candidates(false)? {
            let index = ((u16::from(candidate.ip.octets()[2]) << 8)
                | u16::from(candidate.ip.octets()[3])) as usize
                - 2;
            next_ip_range[index].0 += u16::MAX as usize; // push used IPs to the end of the list
        }

        for assignment in self.read_all_ip_assignments(true)? {
            let index = ((u16::from(assignment.ip.octets()[2]) << 8)
                | u16::from(assignment.ip.octets()[3])) as usize
                - 2;
            next_ip_range[index].0 = 0; // order == 0 => taken
        }

        for candidate in self.read_all_ip_candidates(true)? {
            let index = ((u16::from(candidate.ip.octets()[2]) << 8)
                | u16::from(candidate.ip.octets()[3])) as usize
                - 2;
            next_ip_range[index].0 = 0; // order == 0 => taken
        }

        next_ip_range.sort_by_key(|(order, _)| *order);
        info!("Next IP range order: {:?}", &next_ip_range[..10]);
        next_ip_range
            .iter()
            .find_map(|(order, ip)| {
                if *order == 0 || ip.octets()[3] == 255 {
                    None
                } else {
                    Some(*ip)
                }
            })
            .ok_or_else(|| anyhow::anyhow!("No available IPs"))
    }
}
