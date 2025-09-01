use std::net::Ipv4Addr;
use anyhow::Result;
use pnet_packet::{ip::IpNextHeaderProtocols, Packet, ipv4::Ipv4Packet};
use serde::{Deserialize, Serialize};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use crate::actor::{Action, Handle};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ipv4Pkg(Vec<u8>);

impl From<Ipv4Packet<'static>> for Ipv4Pkg {
    fn from(value: Ipv4Packet<'static>) -> Self {
        Ipv4Pkg(value.packet().to_vec())
    }
}

impl TryFrom<Ipv4Pkg> for Ipv4Packet<'static> {
    type Error = anyhow::Error;

    fn try_from(value: Ipv4Pkg) -> Result<Self> {
        let slice = value.0.into_boxed_slice();
        Ipv4Packet::new(Box::leak(slice))
            .ok_or_else(|| anyhow::anyhow!("Invalid IPv4 packet"))
    }
}


impl Ipv4Pkg {
    pub fn new(buf: &Vec<u8>) -> Result<Self> {
        let pkg = Ipv4Pkg(buf.clone());
        pkg.to_ipv4_packet()?; // validate
        Ok(pkg)
    }

    pub fn to_ipv4_packet(&self) -> Result<Ipv4Packet<'static>> {
        self.clone().try_into()
    }
}

pub struct Tun {
    api: Handle<TunActor>,
}

struct TunActor {
    ip: Ipv4Addr,
    dev: AsyncDevice,
    rx: tokio::sync::mpsc::Receiver<Action<TunActor>>,
    to_remote_writer: tokio::sync::mpsc::Sender<Ipv4Pkg>,
}

impl Tun {
    pub fn new(
        free_ip_ending: (u8, u8),
        to_remote_writer: tokio::sync::mpsc::Sender<Ipv4Pkg>,
    ) -> Result<Self> {
        let ip = Ipv4Addr::new(172, 22, free_ip_ending.0, free_ip_ending.1);
        let dev = DeviceBuilder::new()
            // .name("feth0")
            .ipv4(ip, 24, None)
            .layer(Layer::L3)
            .mtu(2400)
            .build_async()?;

        let (api, rx) = Handle::<TunActor>::channel(1024);

        tokio::spawn(async move {
            let mut actor = TunActor {
                ip,
                to_remote_writer,
                dev,
                rx,
            };
            let _ = actor.run().await;
        });

        Ok(Self { api })
    }

    pub async fn write(&self, pkg: Ipv4Pkg) -> Result<()> {
        self.api.call(move |actor| Box::pin(actor.write_to_tun(pkg))).await
    }
}

impl TunActor {
    async fn run(&mut self) -> Result<()> {
        let mut dev_buf = [0u8; 1024*128];
        loop {
            tokio::select! {

                // Handle API actions
                Some(action) = self.rx.recv() => {
                    action(self).await;
                }
                Ok(len) = self.dev.recv(&mut dev_buf) => {
                    println!("tun_recv-size: {}", len);
                    if let Ok(ip_pkg) = Ipv4Pkg::new(&dev_buf[..len].to_vec()) {
                        let ip_pkg = ip_pkg.to_ipv4_packet().expect("this should have been validated during 'Ipv4Pkg::new' creation");
                        if matches!(
                            ip_pkg.get_next_level_protocol(),
                            IpNextHeaderProtocols::Tcp | IpNextHeaderProtocols::Udp
                        ) {
                            // if packet is ment for local ip: write
                            // if ment for ip of remote node: vpn -> write

                            if ip_pkg.get_destination() == self.ip {
                                let _ = self.dev.send(ip_pkg.packet()).await;
                            } else {
                                let _ = self.to_remote_writer.send(ip_pkg.into()).await;
                            }

                            /*println!(
                                "{} {} {}",
                                ip_pkt.get_next_level_protocol(),
                                ip_pkt.get_source(),
                                ip_pkt.get_destination(),
                            );*/
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
        self.dev.send(pkg.to_ipv4_packet()?.packet()).await?;
        Ok(())
    }
}
