use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("172.22.0.10:8000").await?;
    while let Ok((mut stream, addr)) = listener.accept().await {
        println!("Accepted connection from {}", addr);
        tokio::spawn(async move {
            let (mut reader, mut writer) = stream.split();
            let mut buf = [0u8; 45];
            while let Ok(n) = reader.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if writer.write(&buf[..n]).await.is_err() {
                    break;
                }
                println!("echoed {} bytes to {}", n, addr);
            }
        });
    }

    Ok(())
}
