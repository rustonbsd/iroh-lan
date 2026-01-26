use anyhow::Result;
use pnet_packet::{
    MutablePacket, Packet,
    ip::IpNextHeaderProtocols,
    ipv4::{Ipv4Packet, MutableIpv4Packet, checksum},
    tcp::{MutableTcpPacket, ipv4_checksum as tcp_ipv4_checksum},
    udp::{MutableUdpPacket, ipv4_checksum as udp_ipv4_checksum},
};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Debug,
    net::Ipv4Addr,
    time::{Duration, Instant},
};
use tracing::{info, trace, warn};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use actor_helper::{Action, Handle, act};

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
    tun_tx: tokio::sync::mpsc::Sender<Ipv4Pkg>,
}

struct TunActor {
    ip: Ipv4Addr,
    dev: AsyncDevice,
    rx: actor_helper::Receiver<Action<TunActor>>,
    tun_rx: tokio::sync::mpsc::Receiver<Ipv4Pkg>,
    to_remote_writer: tokio::sync::mpsc::Sender<Ipv4Pkg>,
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
        free_ip_ending: (u8, u8),
        to_remote_writer: tokio::sync::mpsc::Sender<Ipv4Pkg>,
    ) -> Result<Self> {
        let ip = Ipv4Addr::new(172, 22, free_ip_ending.0, free_ip_ending.1);

        #[cfg(target_os = "windows")]
        dll_export().await?;

        let dev = DeviceBuilder::new()
            .ipv4(ip, 24, None)
            .layer(Layer::L3)
            .mtu(1280)
            .build_async()?;

        let (api, rx) = Handle::channel();
        let (tun_tx, tun_rx) = tokio::sync::mpsc::channel(1024 * 16);

        tokio::spawn(async move {
            let mut actor = TunActor {
                ip,
                to_remote_writer: to_remote_writer.clone(),
                dev,
                rx,
                tun_rx,
            };
            let _ = actor.run().await;
        });

        Ok(Self { api, tun_tx })
    }

    pub async fn write(&self, pkg: Ipv4Pkg) -> Result<()> {
        match self.tun_tx.send(pkg).await {
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
    async fn run(&mut self) -> Result<()> {
        let mut dev_buf = [0u8; 1024 * 128];
        info!("TunActor started for IP: {}", self.ip);
        loop {
            tokio::select! {
                Ok(action) = self.rx.recv_async() => {
                    action(self).await;
                }

                // Write path: Network -> TUN
                Some(pkg) = self.tun_rx.recv() => {
                    if let Ok(packet) = pkg.to_ipv4_packet() {
                        let data = packet.packet();
                        match self.dev.send(data).await {
                            Ok(_) => {}
                            Err(e) => warn!("Failed to write to TUN: {}", e),
                        }
                    }
                }

                // Read path: TUN -> Network
                Ok(len) = self.dev.recv(&mut dev_buf) => {
                    if let Some(mut ipv4_packet) = MutableIpv4Packet::new(&mut dev_buf[..len]) {
                        let source = ipv4_packet.get_source();
                        let destination = ipv4_packet.get_destination();

                        // drop broadcast packets to prevent loops (broadcast storms)
                        if destination.octets()[3] == 255 {
                            trace!(
                                "Dropping broadcast packet to prevent loop: src={} dst={}",
                                source,
                                destination
                            );
                            continue;
                        }

                        if !matches!(
                            ipv4_packet.get_next_level_protocol(),
                            IpNextHeaderProtocols::Tcp | IpNextHeaderProtocols::Udp | IpNextHeaderProtocols::Icmp
                        ) {
                            trace!("Ignored packet protocol: {:?}", ipv4_packet.get_next_level_protocol());
                            continue;
                        }

                        // re-calculate and set checksum
                        ipv4_packet.set_checksum(checksum(&ipv4_packet.to_immutable()));

                        match ipv4_packet.get_next_level_protocol() {
                            IpNextHeaderProtocols::Tcp => {
                                if let Some(mut tcp_packet) =
                                    MutableTcpPacket::new(ipv4_packet.payload_mut())
                                {
                                    tcp_packet.set_checksum(tcp_ipv4_checksum(
                                        &tcp_packet.to_immutable(),
                                        &source,
                                        &destination,
                                    ));
                                }
                            }
                            IpNextHeaderProtocols::Udp => {
                                if let Some(mut udp_packet) =
                                    MutableUdpPacket::new(ipv4_packet.payload_mut())
                                {
                                    udp_packet.set_checksum(udp_ipv4_checksum(
                                        &udp_packet.to_immutable(),
                                        &source,
                                        &destination,
                                    ));
                                }
                            }
                            _ => {}
                        }

                        if let Ok(pkg) = Ipv4Pkg::new(ipv4_packet.packet()) {
                            let start = Instant::now();
                            if let Err(e) = self.to_remote_writer.send(pkg).await {
                                warn!("Failed to forward packet from TUN to network: {}", e);
                            } else if start.elapsed() > Duration::from_millis(5) {
                                warn!("TUN->network backpressure: send blocked {} ms", start.elapsed().as_millis());
                            }
                        }
                    } else {
                        warn!("Failed to parse packet from TUN");
                    }
                }

                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl-C received, shutting down TunActor");
                    break
                }
            }
        }
        self.close().await?;
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        info!("Closing TunActor");
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
