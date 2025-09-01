use std::{ net::TcpStream, time::Duration};

use iroh::SecretKey;
use iroh_lan::DirectMessage;
use tokio::{
    io::AsyncReadExt,
    io::AsyncWriteExt,
    time::sleep,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    let secret = SecretKey::generate(&mut rand::thread_rng());

    let router = iroh_lan::Router::builder()
        .entry_name("my-lan-party")
        .secret_key(secret.clone())
        .build()
        .await?;

    while router.node_id_to_ip(router.node_id()).await.is_err() {
        sleep(Duration::from_secs(1)).await;
    }

    let my_ip = router.node_id_to_ip(router.node_id()).await?;

    let (remote_writer, mut remote_reader) = tokio::sync::mpsc::channel(1024);
    let tun = iroh_lan::Tun::new(
        (my_ip.octets()[2], my_ip.octets()[3]),
        remote_writer,
    )?;
    let mut direct_rx = router.subscribe_direct_connect();

    /*
    tokio::spawn(async move {
        sleep(Duration::from_secs(10)).await;
        println!("Connecting to echo server...");
        let local: std::net::SocketAddr = "172.22.0.3:0".parse().unwrap();
        let remote: std::net::SocketAddr = "172.22.0.2:8000".parse().unwrap();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(local).unwrap();
        let mut tcp_stream = socket.connect(remote).await.unwrap();
        //let mut tcp_stream = TcpStream::connect(remote).unwrap();
        println!("Connected to echo server, local addr: {}", tcp_stream.local_addr().unwrap());
        println!(
            "Connected from {} to {}",
            tcp_stream.local_addr().unwrap(),
            tcp_stream.peer_addr().unwrap()
        );

        let (mut receiver, mut sender) = tcp_stream.split();
        let _ =  sender.write_u32_le(8).await;
        let buf = [222u8; 8];
        let _ = sender.write(&buf).await;

        while let Ok(frame_size) = receiver.read_u32_le().await {
            if frame_size == 0 {
                println!("Connection closed frame size == 0");
                break;
            }
            
            let mut buf = Vec::with_capacity(frame_size as usize);
            if receiver.read(buf.as_mut_slice()).await.is_err() {
                println!("failed to read from stream, closing");
                break;
            } else {
                println!("read {} bytes from echo server", frame_size);
            }
            sleep(Duration::from_millis(1000)).await;
            let _ = sender.write_u32_le(frame_size).await;
            if sender.write(&buf).await.is_err() {
                println!("failed to write to stream, closing");
                break;
            }
            println!("echoed {} bytes to {}", frame_size, local);
        }
    });
    */

    println!("My IP: {}", my_ip);

    loop {
        tokio::select! {
            Some(tun_recv) = remote_reader.recv() => {
                if let Ok(remote_node_id)  = router.ip_to_node_id(tun_recv.clone()).await {
                    if let Err(err) = router.direct.route_packet(remote_node_id, DirectMessage::IpPacket(tun_recv)).await {
                        println!("[ERROR] failed to route packet to {:?}", remote_node_id);
                        println!("Reason: {:?}", err);
                    } else {
                        println!("Routed packet to {:?}", remote_node_id);
                    }
                }
            }
            Ok(direct_msg) = direct_rx.recv() => {
                match direct_msg {
                    DirectMessage::IpPacket(ip_pkg) => {
                        println!("WRITE TUN: {:?}", ip_pkg.to_ipv4_packet()?.get_destination());
                        tun.write(ip_pkg).await?;
                    }
                }
            }
        }
    }
}
