use anyhow::{Result, bail};
use iroh::NodeId;
use pnet_packet::{
    Packet,
    ipv4::Ipv4Packet,
};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use pnet_packet::ip::IpNextHeaderProtocols;

#[derive(Debug, Clone, PartialEq, Eq, Hash,Serialize,Deserialize)]
pub struct Ipv4Pkg(Vec<u8>);

impl From<Ipv4Packet<'static>> for Ipv4Pkg {
    fn from(value: Ipv4Packet<'static>) -> Self {
        Ipv4Pkg(value.packet().to_vec())
    }
}

impl From<Ipv4Pkg> for Ipv4Packet<'static> {
    fn from(value: Ipv4Pkg) -> Self {
        let slice = value.0.into_boxed_slice();
        Ipv4Packet::new(Box::leak(slice))
            .expect("Ipv4Pkg is only created via Ipv4Packet so this should be compatible")
    }
}

impl Ipv4Pkg {
    pub fn to_ipv4_packet(&self) -> Ipv4Packet<'static> {
        self.clone().into()
    }
}

pub struct Tun {
    node_id: NodeId,
    ip: Ipv4Addr,
    inner: TunInner,
}

#[derive(Debug)]
struct TunInner {
    ip: Ipv4Addr,
    tun_writer: tokio::sync::broadcast::Sender<Ipv4Pkg>,
    remote_writer: tokio::sync::broadcast::Sender<Ipv4Pkg>,
    _keepalive_tun_reader: tokio::sync::broadcast::Receiver<Ipv4Pkg>,
    _keepalive_remote_reader: tokio::sync::broadcast::Receiver<Ipv4Pkg>,
}

impl Clone for TunInner {
    fn clone(&self) -> Self {
        Self {
            ip: self.ip,
            tun_writer: self.tun_writer.clone(),
            remote_writer: self.remote_writer.clone(),
            _keepalive_tun_reader: self.tun_writer.subscribe(),
            _keepalive_remote_reader: self.remote_writer.subscribe(),
        }
    }
}

impl Tun {
    pub fn new(free_ip_ending: (u8, u8), node_id: NodeId, remote_writer: tokio::sync::broadcast::Sender<Ipv4Pkg>) -> Result<Self> {
        let ip = Ipv4Addr::new(172, 22, free_ip_ending.0, free_ip_ending.1);
        let dev = DeviceBuilder::new()
            // .name("feth0")
            .ipv4(ip, 24, None)
            .layer(Layer::L3)
            .mtu(1400)
            .build_async()?;

        let inner = TunInner::new(ip,remote_writer);
        inner.spawn(dev)?;

        Ok(Self { node_id, ip, inner })
    }

    pub async fn write(&self, pkg: Ipv4Pkg) -> Result<()> {
        self.inner.write_tun(pkg).await
    }
}

impl TunInner {
    pub fn new(ip: Ipv4Addr, remote_writer: tokio::sync::broadcast::Sender<Ipv4Pkg>) -> Self {
        let (tun_writer, mut tun_reader) = tokio::sync::broadcast::channel(1024);
        let _keepalive_tun_writer = tun_writer.subscribe();
        let _keepalive_remote_reader = remote_writer.subscribe();

        // keep sender alive by having a single receiver at all times
        tokio::spawn({
            async move {
                while tun_reader.recv().await.is_ok() {}
                println!("debug: TunInner channel rx dropped");
            }
        });

        Self {
            tun_writer,
            remote_writer,
            ip,
            _keepalive_tun_reader: _keepalive_tun_writer,
            _keepalive_remote_reader,
        }
    }

    pub fn spawn(&self, dev: AsyncDevice) -> Result<()> {
        let inner = self.clone();
        tokio::spawn(async move {
            let mut len = 0;
            let mut buf = [0u8; 65536 + 14];
            let mut remote_rx = inner.remote_writer.subscribe();
            let mut tun_rx = inner.tun_writer.subscribe();
            loop {
                tokio::select! {
                    recv = dev.recv(&mut buf) => {
                        if let Ok(_len) = handle_recv(recv,&buf,len,&inner).await {
                            len = _len;
                        } else {
                            println!("[ERROR] dev.recv failed");
                        }
                    }
                    remote_recv = remote_rx.recv() => {
                        if let Ok(pkg) = remote_recv {
                            let _ = inner.write_tun(pkg).await;
                        } 
                    }
                    tun_recv = tun_rx.recv() => {
                        if let Ok(pkg) = tun_recv {
                            let _ = dev.send(pkg.0.as_slice()).await;
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        ctrl_c();
                        break
                    }
                }
            }
        });

        async fn handle_recv(
            recv: Result<usize, std::io::Error>,
            buf: &[u8],
            len: usize,
            inner: &TunInner,
        ) -> Result<usize> {
            let mut len = len;
            if let Ok(size) = recv {
                len += size;
                if let Some(ip_pkt) = pnet_packet::ipv4::Ipv4Packet::new(&buf[..len]) {
                    if matches!(
                        ip_pkt.get_next_level_protocol(),
                        IpNextHeaderProtocols::Tcp | IpNextHeaderProtocols::Udp
                    ) {
                        // if packet is ment for local ip: write
                        // if ment for ip of remote node: vpn -> write

                        let pkg = Ipv4Pkg(ip_pkt.packet().to_vec());
                        if ip_pkt.get_destination() == inner.ip {
                            let _ = inner.write_tun(pkg).await;
                        } else {
                            let _ = inner.write_remote(pkg).await;
                        }

                        /*println!(
                            "{} {} {}",
                            ip_pkt.get_next_level_protocol(),
                            ip_pkt.get_source(),
                            ip_pkt.get_destination(),
                        );*/
                    }
                    return Ok(0);
                } else {
                    return Ok(len);
                }
            }
            bail!("failed to handle")
        }

        fn ctrl_c() {
            println!("quitting");
        }
        Ok(())
    }

    pub async fn write_tun(&self, pkg: Ipv4Pkg) -> Result<()> {
        self.tun_writer.send(pkg)?;
        Ok(())
    }

    pub async fn write_remote(&self, pkg: Ipv4Pkg) -> Result<()> {
        self.remote_writer.send(pkg)?;
        Ok(())
    }
}
