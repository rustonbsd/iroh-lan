use anyhow::{Context, Result};
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream, VarInt},
};
use sha2::Digest;
use tokio::time::{Duration, timeout};
use tracing::{debug, warn};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn open(
    conn: &iroh::endpoint::Connection,
    network_secret: &[u8; 64],
    our_endpoint_id: EndpointId,
    their_endpoint_id: EndpointId,
) -> Result<(SendStream, RecvStream)> {
    debug!("Auth open: opening stream to {}", their_endpoint_id);
    let (mut send, mut recv) = conn.open_bi().await.context("failed to open stream")?;

    let mut handshake_ours = sha2::Sha512::new();
    handshake_ours.update(network_secret);
    handshake_ours.update(our_endpoint_id);
    handshake_ours.update(their_endpoint_id);

    let handshake_ours_bytes: [u8; 64] = handshake_ours.finalize().into();
    send.write_all(&handshake_ours_bytes)
        .await
        .context("failed to write handshake")?;
    debug!("Auth open: wrote handshake to {}", their_endpoint_id);

    let mut hadnshake_theirs = sha2::Sha512::new();
    hadnshake_theirs.update(network_secret);
    hadnshake_theirs.update(their_endpoint_id);
    hadnshake_theirs.update(our_endpoint_id);
    let handshake_theirs_bytes: [u8; 64] = hadnshake_theirs.finalize().into();

    let mut buf = [0u8; 64];
    debug!(
        "Auth open: waiting for handshake from {}",
        their_endpoint_id
    );

    match timeout(HANDSHAKE_TIMEOUT, recv.read_exact(&mut buf)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(anyhow::anyhow!("failed to read handshake: {}", e)),
        Err(_) => return Err(anyhow::anyhow!("timed out waiting for handshake")),
    }

    if buf != handshake_theirs_bytes {
        conn.close(VarInt::from_u32(403), "handshake failed".as_bytes());
        warn!(
            "Handshake failed: expected {:x?}, got {:x?}",
            handshake_theirs_bytes, buf
        );
        return Err(anyhow::anyhow!(
            "handshake bytes did not match, closing connection"
        ));
    }

    Ok((send, recv))
}

pub async fn accept(
    conn: &iroh::endpoint::Connection,
    network_secret: &[u8; 64],
    our_endpoint_id: EndpointId,
    their_endpoint_id: EndpointId,
) -> Result<(SendStream, RecvStream)> {
    debug!("Auth accept: waiting for stream from {}", their_endpoint_id);
    let (mut send, mut recv) = conn.accept_bi().await.context("failed to accept stream")?;

    let mut hadnshake_theirs = sha2::Sha512::new();
    hadnshake_theirs.update(network_secret);
    hadnshake_theirs.update(their_endpoint_id);
    hadnshake_theirs.update(our_endpoint_id);
    let handshake_theirs_bytes: [u8; 64] = hadnshake_theirs.finalize().into();

    let mut buf = [0u8; 64];
    debug!(
        "Auth accept: waiting for handshake from {}",
        their_endpoint_id
    );
    match timeout(HANDSHAKE_TIMEOUT, recv.read_exact(&mut buf)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(anyhow::anyhow!("failed to read handshake: {}", e)),
        Err(_) => return Err(anyhow::anyhow!("timed out waiting for handshake")),
    }

    if buf != handshake_theirs_bytes {
        conn.close(VarInt::from_u32(403), "handshake failed".as_bytes());
        warn!(
            "Handshake failed: expected {:x?}, got {:x?}",
            handshake_theirs_bytes, buf
        );
        return Err(anyhow::anyhow!(
            "handshake bytes did not match, closing connection"
        ));
    }

    let mut handshake_ours = sha2::Sha512::new();
    handshake_ours.update(network_secret);
    handshake_ours.update(our_endpoint_id);
    handshake_ours.update(their_endpoint_id);
    let handshake_ours_bytes: [u8; 64] = handshake_ours.finalize().into();
    send.write_all(&handshake_ours_bytes)
        .await
        .context("failed to write handshake")?;
    debug!("Auth accept: wrote handshake to {}", their_endpoint_id);

    Ok((send, recv))
}
