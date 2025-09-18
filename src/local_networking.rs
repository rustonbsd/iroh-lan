use anyhow::Result;
use pnet_packet::{Packet, ip::IpNextHeaderProtocols, ipv4::Ipv4Packet};
use serde::{Deserialize, Serialize};
use tracing::debug;
use std::{fmt::Debug, net::Ipv4Addr};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use actor_helper::{act, Action, Handle};

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
    api: Handle<TunActor>,
}

struct TunActor {
    ip: Ipv4Addr,
    dev: AsyncDevice,
    rx: tokio::sync::mpsc::Receiver<Action<TunActor>>,
    to_remote_writer: tokio::sync::broadcast::Sender<Ipv4Pkg>,
    _keep_alive_to_remote_writer: tokio::sync::broadcast::Receiver<Ipv4Pkg>,
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
    pub fn new(
        free_ip_ending: (u8, u8),
        to_remote_writer: tokio::sync::broadcast::Sender<Ipv4Pkg>,
    ) -> Result<Self> {
        let ip = Ipv4Addr::new(172, 22, free_ip_ending.0, free_ip_ending.1);
        let dev = DeviceBuilder::new()
            // .name("feth0")
            .ipv4(ip, 24, None)
            .layer(Layer::L3)
            .mtu(1280)
            .build_async()?;

        let (api, rx) = Handle::<TunActor>::channel(1024*16);

        tokio::spawn(async move {
            let mut actor = TunActor {
                ip,
                to_remote_writer: to_remote_writer.clone(),
                dev,
                rx,
                _keep_alive_to_remote_writer: to_remote_writer.subscribe(),
            };
            let _ = actor.run().await;
        });

        Ok(Self { api })
    }

    pub async fn write(&self, pkg: Ipv4Pkg) -> Result<()> {
        self.api
            .call(act!(actor => actor.write_to_tun(pkg)))
            .await
    }

    pub async fn subscribe(&self) -> Result<tokio::sync::broadcast::Receiver<Ipv4Pkg>> {
        self.api
            .call(act!(actor => actor.subscribe()))
            .await
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
        loop {
            tokio::select! {

                // Handle API actions
                Some(action) = self.rx.recv() => {
                    action(self).await;
                }
                Ok(len) = self.dev.recv(&mut dev_buf) => {
                    if let Ok(ip_pkg) = Ipv4Pkg::new(&dev_buf[..len]) {
                        let ip_pkg = ip_pkg.to_ipv4_packet().expect("this should have been validated during 'Ipv4Pkg::new' creation");
                        if matches!(
                            ip_pkg.get_next_level_protocol(),
                            IpNextHeaderProtocols::Tcp | IpNextHeaderProtocols::Udp | IpNextHeaderProtocols::Icmp
                        ) {
                            // if packet is ment for local ip: write
                            // if ment for ip of remote node: vpn -> write

                            debug!(
                                "{} {} {}",
                                ip_pkg.get_next_level_protocol(),
                                ip_pkg.get_source(),
                                ip_pkg.get_destination(),
                            );

                            if ip_pkg.get_destination() == self.ip {
                                debug!("injected in local tun");
                                let _ = self.dev.send(ip_pkg.packet()).await;
                            } else {
                                let _res = self.to_remote_writer.send(ip_pkg.into());

                               debug!("forwarding_to_remote_writer {}", _res.is_ok());
                            }

                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    break
                }
            }
        }
        Ok(())
    }

    // validated write
    pub async fn write_to_tun(&self, pkg: Ipv4Pkg) -> Result<()> {
        let data = pkg.to_ipv4_packet()?.packet().to_vec();
        let _l = self.dev.send(&data).await?;
        debug!("tun_send-size: {} {}", data.len(), _l);
        Ok(())
    }

    pub async fn subscribe(&mut self) -> Result<tokio::sync::broadcast::Receiver<Ipv4Pkg>> {
        Ok(self.to_remote_writer.subscribe())
    }

    pub async fn close(&mut self) -> Result<()> {
        let _ = &self.dev;
        Ok(())
    }
}
