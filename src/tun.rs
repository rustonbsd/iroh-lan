use anyhow::Result;
use iroh::Endpoint;
use pnet_packet::{
    MutablePacket, Packet,
    icmp::{MutableIcmpPacket, checksum as icmp_checksum},
    ip::IpNextHeaderProtocols,
    ipv4::{Ipv4Flags, Ipv4Packet, MutableIpv4Packet, checksum},
    tcp::{MutableTcpPacket, TcpPacket, ipv4_checksum as tcp_ipv4_checksum},
    udp::{MutableUdpPacket, UdpPacket, ipv4_checksum as udp_ipv4_checksum},
};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Debug,
    net::Ipv4Addr,
    time::{Duration, Instant},
};
use tracing::{debug, info, trace, warn};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use actor_helper::{Action, Handle, Receiver, act};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TunEvent {
    Configure { ip: Ipv4Addr },
    Reconfigure { from: Ipv4Addr, to: Ipv4Addr },
    Teardown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ipv4Pkg(Vec<u8>);

impl<'a> From<Ipv4Packet<'a>> for Ipv4Pkg {
    fn from(value: Ipv4Packet<'a>) -> Self {
        Ipv4Pkg(value.packet().to_vec())
    }
}

impl Ipv4Pkg {
    // Accept anything that can be viewed as a byte slice.
    pub fn new<B: AsRef<[u8]>>(buf: B) -> Result<Self> {
        let v = buf.as_ref().to_vec();
        let pkg = Ipv4Pkg(v);
        // validate
        pkg.to_ipv4_packet()?;
        Ok(pkg)
    }

    // Borrowing view over the internal bytes.
    pub fn to_ipv4_packet(&self) -> Result<Ipv4Packet<'_>> {
        Ipv4Packet::new(&self.0).ok_or_else(|| anyhow::anyhow!("Invalid IPv4 packet"))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct Tun {
    api: Handle<TunActor, anyhow::Error>,
    inner_remote_to_tun_tx: tokio::sync::mpsc::Sender<Ipv4Pkg>,
}

struct TunActor {
    ip: Ipv4Addr,
    dev: AsyncDevice,
    inner_remote_to_tun_rx: tokio::sync::mpsc::Receiver<Ipv4Pkg>,
    tun_to_remote_tx: tokio::sync::mpsc::Sender<Ipv4Pkg>,
    endpoint: Endpoint,
}

impl Debug for TunActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunActor")
            .field("ip", &self.ip)
            .field("dev", &"AsyncDevice")
            .finish()
    }
}

impl Tun {
    pub async fn new(
        tun_ip: Ipv4Addr,
        tun_to_remote_tx: tokio::sync::mpsc::Sender<Ipv4Pkg>,
        endpoint: Endpoint,
    ) -> Result<Self> {
        #[cfg(target_os = "windows")]
        dll_export().await?;

        let dev = DeviceBuilder::new()
            .ipv4(tun_ip, 16, None)
            .layer(Layer::L3)
            .mtu(1400)
            .build_async()?;

        let (inner_remote_to_tun_tx, inner_remote_to_tun_rx) =
            tokio::sync::mpsc::channel(1024 * 16);
        let (api, _) = Handle::spawn_with(
            TunActor {
                ip: tun_ip,
                tun_to_remote_tx,
                inner_remote_to_tun_rx,
                dev,
                endpoint,
            },
            |mut actor, rx| async move { actor.run(rx).await },
        );
        Ok(Self {
            api,
            inner_remote_to_tun_tx,
        })
    }

    pub async fn write(&self, pkg: Ipv4Pkg) -> Result<()> {
        let cap = self.inner_remote_to_tun_tx.capacity();
        if cap < 1000 {
            warn!(
                "TunActor write channel saturated ({} free). Packet flow stalled.",
                cap
            );
        }

        match self.inner_remote_to_tun_tx.send(pkg).await {
            Ok(_) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("Tun actor closed: {}", e)),
        }
    }

    pub async fn close(&self) -> Result<()> {
        self.api
            .call(act!(actor =>
                actor.close()
            ))
            .await
    }
}

impl TunActor {
    async fn run(&mut self, rx: Receiver<Action<TunActor>>) -> Result<()> {
        let mut socket_binding_timer = tokio::time::interval(Duration::from_secs(5));
        socket_binding_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut bound_socket_ports = Vec::new();

        let mut dev_buf = [0u8; 1024 * 128];
        info!("TunActor started for IP: {}", self.ip);
        loop {
            tokio::select! {
                Ok(action) = rx.recv_async() => {
                    action(self).await;
                }

                // Write path: Network -> TUN
                Some(pkg) = self.inner_remote_to_tun_rx.recv() => {
                    if let Ok(packet) = pkg.to_ipv4_packet() {
                        let data = packet.packet();

                        // logging
                        let protocol = packet.get_next_level_protocol();
                        let dest = packet.get_destination();
                        let src = packet.get_source();
                        match packet.get_next_level_protocol() {
                            IpNextHeaderProtocols::Tcp => {
                                if let Some(tcp) = TcpPacket::new(packet.payload()) {
                                    let src_port = tcp.get_source();
                                    let dst_port = tcp.get_destination();
                                    debug!(%protocol, %dest, %src, %src_port, %dst_port, "received from peer");
                                }
                            }
                            IpNextHeaderProtocols::Udp => {
                                if let Some(udp) = UdpPacket::new(packet.payload()) {
                                    let src_port = udp.get_source();
                                    let dst_port = udp.get_destination();
                                    debug!(%protocol, %dest, %src, %src_port, %dst_port, "received from peer");
                                }
                            }
                            _ => {
                                debug!(%protocol, %dest, %src, "received from peer (not TCP/UDP)");
                            }
                        }

                        if dest != self.ip {
                            warn!(%dest,"dropping packet that are not meant for us");
                            continue;
                        }
                        if src == self.ip {
                            warn!(%src, "dropping packet authored by us");
                            continue;
                        }
                        if let Err(err) = self.dev.send(data).await {
                            warn!(%protocol, %dest, %src, ?err, "failed to write packet to TUN device");
                        }
                    }
                }

                // Read path: TUN -> Network
                Ok(len) = self.dev.recv(&mut dev_buf) => {
                    if let Some(mut packet) = MutableIpv4Packet::new(&mut dev_buf[..len]) {
                        let src = packet.get_source();
                        let dest = packet.get_destination();
                        let protocol = packet.get_next_level_protocol();

                        // drop broadcast packets to prevent loops (broadcast storms)
                        if dest.octets()[2] == 255 && dest.octets()[3] == 255 {
                            trace!(
                                %protocol, %dest, %src, "dropping broadcast packet to *.*.*.255 to prevent loop"
                            );
                            continue;
                        }

                        if !matches!(
                            packet.get_next_level_protocol(),
                            IpNextHeaderProtocols::Tcp | IpNextHeaderProtocols::Udp | IpNextHeaderProtocols::Icmp
                        ) {
                            trace!(%protocol, %dest, %src, "ignored packet protocol");
                            continue;
                        }

                        // re-calculate and set checksum for the IP header
                        packet.set_checksum(checksum(&packet.to_immutable()));

                        // Check if this is a fragment
                        // If offset > 0, it's a tail fragment (no L4 header).
                        // If MF (MoreFragments) flag is set, it's a head fragment (checksum covers future data we don't have).
                        let is_fragment = (packet.get_flags() & Ipv4Flags::MoreFragments) != 0
                            || packet.get_fragment_offset() > 0;

                        if !is_fragment {
                            // ONLY calculate L4 checksums for whole packets
                            match packet.get_next_level_protocol() {
                                IpNextHeaderProtocols::Tcp => {
                                    if let Some(mut tcp_packet) =
                                        MutableTcpPacket::new(packet.payload_mut())
                                    {
                                        tcp_packet.set_checksum(tcp_ipv4_checksum(
                                            &tcp_packet.to_immutable(),
                                            &src,
                                            &dest,
                                        ));
                                    }
                                }
                                IpNextHeaderProtocols::Udp => {
                                    if let Some(mut udp_packet) =
                                        MutableUdpPacket::new(packet.payload_mut())
                                    {
                                        if bound_socket_ports.contains(&udp_packet.get_source()) {
                                            warn!(%protocol, %src, %dest, "filtered iroh UDP packet from socket port");
                                            continue;
                                        }

                                        udp_packet.set_checksum(udp_ipv4_checksum(
                                            &udp_packet.to_immutable(),
                                            &src,
                                            &dest,
                                        ));
                                    }
                                }
                                IpNextHeaderProtocols::Icmp => {
                                    if let Some(mut icmp_packet) =
                                        MutableIcmpPacket::new(packet.payload_mut())
                                    {
                                        icmp_packet.set_checksum(icmp_checksum(
                                            &icmp_packet.to_immutable(),
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }

                        if let Ok(pkg) = Ipv4Pkg::new(packet.packet()) {
                            let send_timer = Instant::now();
                            if let Err(err) = self.tun_to_remote_tx.send(pkg).await {
                                warn!(%protocol, %dest, %src, ?err, "failed to forward packet from TUN to network");
                            } else if send_timer.elapsed() > Duration::from_millis(5) {
                                warn!(%protocol, %dest, %src, "TUN->network backpressure: send blocked {} ms", send_timer.elapsed().as_millis());
                            }
                        }
                    } else {
                        warn!("failed to parse packet from TUN");
                    }
                }

                _ = socket_binding_timer.tick() => {
                    bound_socket_ports = self.endpoint
                        .bound_sockets()
                        .iter()
                        .filter_map(|addr: &std::net::SocketAddr| if addr.ip().is_ipv4() { Some(addr.port()) } else { None })
                        .collect();
                }

                else => break,
            }
        }
        self.close().await?;
        Ok(())
    }
}

impl TunActor {
    pub async fn close(&mut self) -> Result<()> {
        info!("closing TunActor");
        let _ = &self.dev;
        Ok(())
    }
}

#[cfg(all(windows, target_arch = "x86"))]
const WINTUN_DLL_EMBEDDED: &[u8] = include_bytes!("../dependencies/wintun/bin/x86/wintun.dll");
#[cfg(all(windows, target_arch = "x86_64"))]
const WINTUN_DLL_EMBEDDED: &[u8] = include_bytes!("../dependencies/wintun/bin/amd64/wintun.dll");
#[cfg(all(windows, target_arch = "aarch64"))]
const WINTUN_DLL_EMBEDDED: &[u8] = include_bytes!("../dependencies/wintun/bin/arm64/wintun.dll");
#[cfg(all(windows, target_arch = "arm"))]
const WINTUN_DLL_EMBEDDED: &[u8] = include_bytes!("../dependencies/wintun/bin/arm/wintun.dll");

#[cfg(target_os = "windows")]
async fn dll_export() -> anyhow::Result<()> {
    let working_dir = std::env::current_exe()?
        .parent()
        .ok_or(anyhow::anyhow!("Failed to get parent directory"))?
        .to_path_buf();
    let dll_path = working_dir.join("wintun.dll");
    if tokio::fs::try_exists(dll_path.clone()).await? {
        return Ok(());
    } else {
        tokio::fs::write(&dll_path, WINTUN_DLL_EMBEDDED).await?;
    }
    Ok(())
}
