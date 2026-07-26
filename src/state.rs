use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};

use iroh::{EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use tracing::info;
use std::cmp::Ordering;

use crate::{build_ip, current_time};

type WallMs = u64;
type Frame = Vec<u8>;

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("codec: {0}")]
    Codec(#[from] postcard::Error),
    #[error("bad signature")]
    BadSignature,
    #[error("ip space exhausted")]
    IpSpaceExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Version {
    pub wall_ms: WallMs,
    pub counter: u64,
}

impl Version {
    fn bump(self, now_ms: WallMs) -> Self {
        // never go backwards, even if the clock does
        if now_ms > self.wall_ms {
            Version {
                wall_ms: now_ms,
                counter: 0,
            }
        } else {
            Version {
                wall_ms: self.wall_ms,
                counter: self.counter + 1,
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeState {
    pub endpoint_id: EndpointId,
    pub version: Version,
    pub ip_claim: IpClaim,
    pub exposed_ports: Ports,
}

impl NodeState {
    fn sign(self, sk: &SecretKey) -> Result<SignedNodeState, MeshError> {
        let bin = postcard::to_stdvec(&self)?;
        let signature = sk.sign(&bin);
        Ok(SignedNodeState {
            inner: self,
            signature,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SignedNodeState {
    inner: NodeState,
    signature: Signature,
}

impl SignedNodeState {
    pub fn get(&self) -> &NodeState {
        &self.inner
    }

    fn verify(self) -> Result<Self, MeshError> {
        self.inner
            .endpoint_id
            .verify(&postcard::to_stdvec(&self.inner)?, &self.signature)
            .map_err(|_| MeshError::BadSignature)?;
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum IpClaim {
    Unclaimed,
    Claimed(Claim),
}

impl IpClaim {
    pub fn claim(&self) -> Option<&Claim> {
        match self {
            IpClaim::Unclaimed => None,
            IpClaim::Claimed(c) => Some(c),
        }
    }

    pub fn ip(&self) -> Option<u16> {
        self.claim().map(|c| c.ip)
    }
    pub fn to_ipv4(&self, net_prefix: u16) -> Option<std::net::Ipv4Addr> {
        self.ip().map(|ip| build_ip(net_prefix, ip))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Claim {
    /// host bits within the /16
    pub ip: u16,
    /// which candidate index of our personal sequence this ip is
    pub k: u64,
    /// first claim
    pub claimed_at_ms: WallMs,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Ports {
    Any,
    List(Vec<u16>),
}

#[derive(Debug, Serialize, Deserialize)]
enum Wire {
    FullSync { states: Vec<SignedNodeState> },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MeshEvent {
    Tun(TunDirective),
    PeerChanged {
        peer: EndpointId,
        state: NodeState,
    },
    PeerDisconnected {
        peer: EndpointId,
    },
    ClaimResolved {
        ip: u16,
        winner: EndpointId,
        loser: EndpointId,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TunDirective {
    Configure {
        ip: u16,
    },
    Reconfigure {
        from: u16,
        to: u16,
    },
    /// We lost our claim and have no valid ip yet
    Teardown {
        from: u16,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TunState {
    Down,
    Up { ip: u16 },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub tick_interval: Duration,
    /// Base ip prefix for the mesh network (default: 172.30 => 172u16 << 8 | 30u16 )
    pub net_prefix: u16,
    /// Time until we consider a node disconnected
    pub disconnect_timeout: Duration,
    /// Time without change until we expect to have a settled view of our peers
    pub settle_ticks: u64,
    /// During ip conflict we consider two nodes timestamps identical when they are within this range
    pub simultaneous_range: Duration,
    /// Min peer num before we have a minimum viable network (wait to bring up tun until we have enough peers)
    pub min_viable_network: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_millis(500),
            net_prefix: 172u16 << 8 | 30u16,
            disconnect_timeout: Duration::from_secs(60),
            settle_ticks: 20,
            simultaneous_range: Duration::from_secs(30),
            min_viable_network: 2,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PeerMeta {
    /// TODO: wire into DirectConnect
    connected: bool,
    last_fresh_ms: WallMs,
}

#[derive(Debug)]
pub struct State {
    eid: EndpointId,
    signing_key: SecretKey,
    config: Config,

    /// CRDT: per-author LWW register keyed by endpoint id
    states: BTreeMap<EndpointId, SignedNodeState>,
    peers: HashMap<EndpointId, PeerMeta>,

    tun: TunState,
    /// Ticks since the last view change (new peer / claim change)
    quiet_ticks: u64,

    outbound: Vec<(EndpointId, Frame)>,
    events: Vec<MeshEvent>,
}

/// Public: API
impl State {
    pub fn new(secret_key: &SecretKey, config: Config, now_ms: Option<WallMs>) -> Self {
        let mut mesh = Self {
            eid: secret_key.public(),
            signing_key: secret_key.clone(),
            config,
            states: BTreeMap::new(),
            peers: HashMap::new(),
            tun: TunState::Down,
            quiet_ticks: 0,
            outbound: Vec::new(),
            events: Vec::new(),
        };
        // start unclaimed, we don't pick an ip until we've seen the network.
        mesh.write_own(now_ms.unwrap_or_else(current_time), |state| {
            state.ip_claim = IpClaim::Unclaimed;
            state.exposed_ports = Ports::Any;
        });
        mesh
    }

    /// for later if we want to add persistence
    #[allow(dead_code)]
    pub fn resume(
        secret_key: &SecretKey,
        config: Config,
        persisted: PersistedState,
        now_ms: WallMs,
    ) -> Self {
        let mut mesh = Self::new(secret_key, config, Some(now_ms));
        mesh.write_own(now_ms, |state| {
            state.ip_claim = IpClaim::Claimed(persisted.claim);
            state.exposed_ports = persisted.exposed_ports;
        });
        mesh
    }

    pub fn endpoint_id(&self) -> &EndpointId {
        &self.eid
    }

    pub fn own(&self) -> &NodeState {
        self.states
            .get(&self.eid)
            .expect("own state always present")
            .get()
    }

    pub fn get_node_states(&self) -> Vec<&NodeState> {
        self.states.values().map(|signed| &signed.inner).collect()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn tick(&mut self, now_ms: WallMs) {
        self.expire_departed(now_ms);

        // bump tick
        self.write_own(now_ms, |_| {});

        self.quiet_ticks = self.quiet_ticks.saturating_add(1);
        self.maybe_claim_ip(now_ms);
        self.send_sync_to_all();
    }

    #[allow(dead_code)]
    pub fn set_exposed_ports(&mut self, ports: Ports, now_ms: WallMs) {
        if self.own().exposed_ports == ports {
            return;
        }
        self.write_own(now_ms, |state| state.exposed_ports = ports);
        self.send_sync_to_all();
    }

    pub fn on_frame(
        &mut self,
        from: EndpointId,
        bytes: &[u8],
        now_ms: WallMs,
    ) -> Result<(), MeshError> {
        let Wire::FullSync { states } = postcard::from_bytes(bytes)?;
        let mut changed = false;
        for signed in states {
            changed |= self.merge_one(signed, now_ms)?;
        }
        if let Some(meta) = self.peers.get_mut(&from) {
            meta.last_fresh_ms = now_ms;
        }
        if changed {
            info!(%from, "received frame");
            self.resolve_conflicts(now_ms);
            self.maybe_claim_ip(now_ms);
        }
        Ok(())
    }

    pub fn on_peer_connected(&mut self, eid: EndpointId, now_ms: WallMs) {
        let meta = self.peers.entry(eid).or_insert(PeerMeta {
            connected: false,
            last_fresh_ms: now_ms,
        });
        meta.last_fresh_ms = now_ms;

        if !std::mem::replace(&mut meta.connected, true) {
            self.reset_quiet_ticks();
        }
        self.send_sync_to(eid);
    }

    pub fn on_peer_disconnected(&mut self, eid: EndpointId) {
        if let Some(meta) = self.peers.get_mut(&eid) {
            meta.connected = false;
        }
    }
}

/// Internals: general
impl State {
    #[must_use]
    pub fn drain_outbound(&mut self) -> Vec<(EndpointId, Frame)> {
        std::mem::take(&mut self.outbound)
    }

    #[must_use]
    pub fn drain_events(&mut self) -> Vec<MeshEvent> {
        std::mem::take(&mut self.events)
    }

    fn write_own(&mut self, now_ms: WallMs, f: impl FnOnce(&mut NodeState)) {
        let mut state = match self.states.get(&self.eid) {
            Some(s) => s.get().clone(),
            None => NodeState {
                endpoint_id: self.eid,
                version: Version {
                    wall_ms: now_ms,
                    counter: 0,
                },
                ip_claim: IpClaim::Unclaimed,
                exposed_ports: Ports::Any,
            },
        };
        f(&mut state);
        state.version = state.version.bump(now_ms);
        let signed = state.sign(&self.signing_key).expect("sign own state");
        self.states.insert(self.eid, signed);
    }

    fn expire_departed(&mut self, now_ms: WallMs) {
        let timeout = self.config.disconnect_timeout.as_millis() as u64;
        let gone: Vec<EndpointId> = self
            .peers
            .iter()
            .filter(|(eid, peer_meta)| {
                **eid != self.eid
                    && !peer_meta.connected
                    && now_ms.saturating_sub(peer_meta.last_fresh_ms) >= timeout
            })
            .map(|(&eid, _)| eid)
            .collect();
        for eid in gone {
            self.peers.remove(&eid);
            self.states.remove(&eid);
            self.events.push(MeshEvent::PeerDisconnected { peer: eid });
            self.reset_quiet_ticks();
        }
    }

    fn reset_quiet_ticks(&mut self) {
        self.quiet_ticks = 0;
    }

    fn maybe_claim_ip(&mut self, now_ms: WallMs) {
        if self.own().ip_claim.claim().is_some() {
            self.sync_tun();
            return;
        }
        let live_peers = self.live_peer_count(now_ms);
        let ready = live_peers >= self.config.min_viable_network
            && self.quiet_ticks >= self.config.settle_ticks;
        if !ready {
            return;
        }
        let _ = self.take_ip(0, now_ms);
    }

    fn live_peer_count(&self, now_ms: WallMs) -> usize {
        let timeout = self.config.disconnect_timeout.as_millis() as u64;
        self.peers
            .iter()
            .filter(|(id, m)| {
                **id != self.eid
                    && (m.connected || now_ms.saturating_sub(m.last_fresh_ms) < timeout)
            })
            .count()
    }

    /// LWW merge for one author, returns true if view changed
    fn merge_one(&mut self, signed: SignedNodeState, now_ms: WallMs) -> Result<bool, MeshError> {
        let signed = signed.verify()?;

        let author = signed.get().endpoint_id;
        if author == self.eid {
            return Ok(false);
        }

        // if same or stale version, no change
        let incoming_version = signed.get().version;
        let old = self.states.get(&author).map(|s| s.get().clone());
        if let Some(old) = &old
            && old.version >= incoming_version
        {
            return Ok(false);
        }

        let semantic_change = match &old {
            Some(prev) => {
                prev.ip_claim != signed.get().ip_claim
                    || prev.exposed_ports != signed.get().exposed_ports
            }
            None => true,
        };

        self.states.insert(author, signed.clone());

        self.peers
            .entry(author)
            .or_insert(PeerMeta {
                connected: false,
                last_fresh_ms: now_ms,
            })
            .last_fresh_ms = now_ms;

        if old.is_none()
            || old.as_ref().map(|p| p.ip_claim.clone()) != Some(signed.get().ip_claim.clone())
        {
            self.reset_quiet_ticks();
        }
        if semantic_change {
            self.events.push(MeshEvent::PeerChanged {
                peer: author,
                state: signed.get().clone(),
            });
        }
        Ok(semantic_change)
    }

    fn resolve_conflicts(&mut self, now_ms: WallMs) {
        let mut by_ip: HashMap<u16, Vec<(EndpointId, Claim)>> = HashMap::new();
        for (eid, signed_state) in &self.states {
            if let Some(claim) = signed_state.get().ip_claim.claim() {
                by_ip.entry(claim.ip).or_default().push((*eid, *claim));
            }
        }

        let mut i_lost: Option<u64> = None;
        for (ip, mut claimants) in by_ip {
            if claimants.len() < 2 {
                continue;
            }
            claimants.sort_by(|a, b| self.claim_priority((&a.0, &a.1), (&b.0, &b.1)));
            let winner = claimants.last().copied().expect("non-empty");
            for (loser_id, loser_claim) in claimants.iter().take(claimants.len() - 1) {
                self.events.push(MeshEvent::ClaimResolved {
                    ip,
                    winner: winner.0,
                    loser: *loser_id,
                });
                if *loser_id == self.eid {
                    i_lost = Some(loser_claim.k.saturating_add(1));
                } else {
                    // drop the losers claim locally they'll re-announce.
                    // this should be fine since we only update peers with newer versions
                    // so stale duplicates should just be ignored
                    if let Some(state) = self.states.get_mut(loser_id) {
                        state.inner.ip_claim = IpClaim::Unclaimed;
                    }
                }
            }
        }

        if let Some(next_k) = i_lost {
            let _ = self.take_ip(next_k, now_ms);
        }
    }

    /// Total order over two claims for the same IP. `std::cmp::Ordering::Greater` wins.
    ///
    /// 1. seniority (older claimed_at wins): protects long-running nodes
    /// 2. lower k wins
    /// 3. higher endpoint id: deterministic tiebreaker
    fn claim_priority(
        &self,
        a: (&EndpointId, &Claim),
        b: (&EndpointId, &Claim),
    ) -> std::cmp::Ordering {
        let (a_eid, a_claim) = a;
        let (b_eid, b_claim) = b;
        let delta = a_claim.claimed_at_ms.abs_diff(b_claim.claimed_at_ms);
        if delta > self.config.simultaneous_range.as_millis() as u64 {
            // older timestamps are smaller and have priority
            // we flip comparison to make the smallest timestamp the winner
            return b_claim.claimed_at_ms.cmp(&a_claim.claimed_at_ms);
        }
        match b_claim.k.cmp(&a_claim.k) {
            Ordering::Equal => a_eid.as_bytes().cmp(b_eid.as_bytes()),
            ord => ord,
        }
    }
}

/// Internals: ip claims stuff
impl State {
    fn take_ip(&mut self, start_k: u64, now_ms: WallMs) -> Result<(), MeshError> {
        let occupied = self.occupied_ips(&self.eid);
        let (ip, k) = (start_k..)
            .map(|k| (Self::candidate(&self.eid, k), k))
            .find(|(ip, _)| !occupied.contains(ip))
            .ok_or(MeshError::IpSpaceExhausted)?;

        self.write_own(now_ms, |state| {
            state.ip_claim = IpClaim::Claimed(Claim {
                ip,
                k,
                claimed_at_ms: now_ms,
            });
        });
        self.sync_tun();
        self.send_sync_to_all();
        Ok(())
    }

    fn occupied_ips(&self, excluding: &EndpointId) -> HashSet<u16> {
        self.states
            .iter()
            .filter(|(eid, _)| *eid != excluding)
            .filter_map(|(_, s)| s.get().ip_claim.ip())
            .collect()
    }

    /// k-th candidate host address for a node. anyone who knows the
    /// endpoint id can compute it. uniform over the space => collision
    /// probability 1/65533 per pair.
    pub fn candidate(endpoint_id: &EndpointId, k: u64) -> u16 {
        let mut h = blake3::Hasher::new();
        h.update(endpoint_id.as_bytes());
        h.update(&k.to_le_bytes());
        let d = h.finalize();
        let raw = u16::from_le_bytes([d.as_bytes()[0], d.as_bytes()[1]]);
        2 + (raw % 65_533) // skip .0.0, .0.1, .255.255
    }
}

/// Internals: sync stuff
impl State {
    fn sync_tun(&mut self) {
        let desired = self.own().ip_claim.ip();
        match (self.tun, desired) {
            (TunState::Down, Some(ip)) => {
                self.tun = TunState::Up { ip };
                self.events
                    .push(MeshEvent::Tun(TunDirective::Configure { ip }));
            }
            (TunState::Up { ip: from }, Some(to)) if from != to => {
                self.tun = TunState::Up { ip: to };
                self.events
                    .push(MeshEvent::Tun(TunDirective::Reconfigure { from, to }));
            }
            (TunState::Up { ip: from }, None) => {
                self.tun = TunState::Down;
                self.events
                    .push(MeshEvent::Tun(TunDirective::Teardown { from }));
            }
            _ => {}
        }
    }

    fn send_sync_to_all(&mut self) {
        let frame = self.sync_frame();
        let targets: Vec<EndpointId> = self
            .peers
            .iter()
            .filter(|(_, peer_meta)| peer_meta.connected)
            .map(|(eid, _)| *eid)
            .collect();
        for peer in targets {
            self.outbound.push((peer, frame.clone()));
        }
    }

    fn sync_frame(&self) -> Frame {
        let wire = Wire::FullSync {
            states: self.states.values().cloned().collect(),
        };
        postcard::to_stdvec(&wire).expect("encode sync")
    }

    fn send_sync_to(&mut self, eid: EndpointId) {
        let frame = self.sync_frame();
        self.outbound.push((eid, frame));
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedState {
    pub claim: Claim,
    pub exposed_ports: Ports,
}
