use anyhow::Result;
use iroh::{
    EndpointId,
    endpoint::{RecvStream, SendStream, VarInt},
};
use sha2::Digest;
use tracing::{debug, warn};

pub async fn open(
    conn: &iroh::endpoint::Connection,
    network_secret: &[u8; 64],
    our_endpoint_id: EndpointId,
    their_endpoint_id: EndpointId,
) -> Result<(SendStream, RecvStream, SendStream, RecvStream)> {
    debug!("Auth open: opening control stream to {}", their_endpoint_id);
    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;

    let mut handshake_ours = sha2::Sha512::new();
    handshake_ours.update(network_secret);
    handshake_ours.update(our_endpoint_id);
    handshake_ours.update(their_endpoint_id);

    let handshake_ours_bytes: [u8; 64] = handshake_ours.finalize().into();
    ctrl_send.write_all(&handshake_ours_bytes).await?;
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
    ctrl_recv.read_exact(&mut buf).await?;
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

    debug!("Auth open: opening data stream to {}", their_endpoint_id);
    let (data_send, data_recv) = conn.open_bi().await?;

    Ok((ctrl_send, ctrl_recv, data_send, data_recv))
}

pub async fn accept(
    conn: &iroh::endpoint::Connection,
    network_secret: &[u8; 64],
    our_endpoint_id: EndpointId,
    their_endpoint_id: EndpointId,
) -> Result<(SendStream, RecvStream, SendStream, RecvStream)> {
    debug!(
        "Auth accept: waiting for control stream from {}",
        their_endpoint_id
    );
    let (mut ctrl_send, mut ctrl_recv) = conn.accept_bi().await?;

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
    ctrl_recv.read_exact(&mut buf).await?;
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
    ctrl_send.write_all(&handshake_ours_bytes).await?;
    debug!("Auth accept: wrote handshake to {}", their_endpoint_id);

    debug!(
        "Auth accept: waiting for data stream from {}",
        their_endpoint_id
    );
    let (data_send, data_recv) = conn.accept_bi().await?;

    Ok((ctrl_send, ctrl_recv, data_send, data_recv))
}
