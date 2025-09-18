use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::sleep,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // bind local addr 172.22.0.3:0 and connect to remote
    let local: std::net::SocketAddr = "172.22.0.4:0".parse()?;
    let remote: std::net::SocketAddr = "172.22.0.3:8000".parse()?;
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind(local)?;
    let mut tcp_stream = socket.connect(remote).await?;
    println!(
        "Connected from {} to {}",
        tcp_stream.local_addr()?,
        tcp_stream.peer_addr()?
    );

    let (mut reader, mut writer) = tcp_stream.split();
    writer.write_all(b"Hello, world!").await?;
    let mut buf = [0u8; 45];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        sleep(Duration::from_millis(1000)).await;
        if writer.write(&buf[..n]).await.is_err() {
            break;
        }
        println!("echoed {} bytes to {}", n, local);
    }

    Ok(())
}
