use iroh::{EndpointId, endpoint::{RecvStream, SendStream, VarInt}};
use anyhow::Result;
use sha2::Digest;
use tracing::warn;

pub async fn open(conn: &iroh::endpoint::Connection, network_secret: &[u8; 64], our_endpoint_id: EndpointId, their_endpoint_id: EndpointId) -> Result<(SendStream, RecvStream)> {

    let (mut send, mut recv) = conn.open_bi().await?;

    let mut handshake_ours = sha2::Sha512::new();
    handshake_ours.update(network_secret);
    handshake_ours.update(our_endpoint_id.as_bytes());
    handshake_ours.update(their_endpoint_id.as_bytes());
    let handshake_ours_bytes: [u8; 64] = handshake_ours.finalize().into();
    send.write_all(&handshake_ours_bytes).await?;

    let mut hadnshake_theirs = sha2::Sha512::new();
    hadnshake_theirs.update(network_secret);
    hadnshake_theirs.update(their_endpoint_id.as_bytes());
    hadnshake_theirs.update(our_endpoint_id.as_bytes());
    let handshake_theirs_bytes: [u8; 64] = hadnshake_theirs.finalize().into();

    let mut buf = [0u8; 64];
    recv.read_exact(&mut buf).await?;
    if buf != handshake_theirs_bytes {
        conn.close(VarInt::from_u32(403), "handshake failed".as_bytes());
        warn!("Handshake failed: expected {:x?}, got {:x?}", handshake_theirs_bytes, buf);
        return Err(anyhow::anyhow!("handshake bytes did not match, closing connection"));
    }

    Ok((send, recv))
}

pub async fn accept(conn: &iroh::endpoint::Connection, network_secret:  &[u8; 64], our_endpoint_id: EndpointId, their_endpoint_id: EndpointId) -> Result<(SendStream, RecvStream)> {

    let (mut send, mut recv) = conn.accept_bi().await?;

    let mut hadnshake_theirs = sha2::Sha512::new();
    hadnshake_theirs.update(network_secret);
    hadnshake_theirs.update(their_endpoint_id.as_bytes());
    hadnshake_theirs.update(our_endpoint_id.as_bytes());
    let handshake_theirs_bytes: [u8; 64] = hadnshake_theirs.finalize().into();

    let mut buf = [0u8; 64];
    recv.read_exact(&mut buf).await?;
    if buf != handshake_theirs_bytes {
        conn.close(VarInt::from_u32(403), "handshake failed".as_bytes());
        warn!("Handshake failed: expected {:x?}, got {:x?}", handshake_theirs_bytes, buf);
        return Err(anyhow::anyhow!("handshake bytes did not match, closing connection"));
    }
    
    let mut handshake_ours = sha2::Sha512::new();
    handshake_ours.update(network_secret);
    handshake_ours.update(our_endpoint_id.as_bytes());
    handshake_ours.update(their_endpoint_id.as_bytes());
    let handshake_ours_bytes: [u8; 64] = handshake_ours.finalize().into();
    send.write_all(&handshake_ours_bytes).await?;

    Ok((send, recv))
}