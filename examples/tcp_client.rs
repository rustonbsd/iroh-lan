use std::time::Duration;

use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream, time::sleep};



#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // bind local addr 172.22.0.3:0 and connect to remote
    let local: std::net::SocketAddr = "172.22.0.3:0".parse()?;
    let remote: std::net::SocketAddr = "172.22.0.2:8000".parse()?;
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind(local)?;
    let mut tcp_stream = socket.connect(remote).await?;

    let (mut reader, mut writer) = tcp_stream.split();
    let mut buf = [0u8; 45];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        sleep(Duration::from_millis(10000)).await;
        if writer.write(&buf[..n]).await.is_err() {
            break;
        }
        println!("echoed {} bytes to {}", n, local);
    }

    Ok(())
}