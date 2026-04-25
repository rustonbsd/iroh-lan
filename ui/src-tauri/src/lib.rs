use iroh_lan::Network;
use once_cell::sync::Lazy;
use serde::Serialize;
use tauri_plugin_log::log;
use tokio::sync::Mutex;
use tracing::info;

// Re-export lib so main.rs can call run()
// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

static NETWORK: Lazy<Mutex<Option<Network>>> = Lazy::new(|| {
    Mutex::new(None)
});

#[derive(Debug, Serialize)]
pub enum Status {
    Active,
    Idle,
    Pending,
    Disconnected,
}

#[derive(Debug, Serialize)]
pub struct PeerInfo {
    pub node_id: String,
    pub ip: String,
    pub status: Status, // currently always Active; placeholder for future states
}

#[derive(Debug, Serialize)]
pub struct MyInfo {
    pub node_id: String,
    pub ip: Option<String>,
}

impl MyInfo {
    pub async fn from_network(network: &Network) -> anyhow::Result<Self> {
        let router = network.get_router_handle().await?;

        let ip_state = router.get_ip_state().await?;
        let node_id = router.get_node_id().await?;

        Ok(Self {
            node_id: node_id.to_string(),
            ip: match ip_state {
                iroh_lan::RouterIp::NoIp => None,
                iroh_lan::RouterIp::AquiringIp(ip_candidate, _) => {
                    Some(format!("acquiring {}...", ip_candidate.ip))
                }
                iroh_lan::RouterIp::AssignedIp(ipv4_addr) => Some(format!("{ipv4_addr}")),
                iroh_lan::RouterIp::VerifyingIp(ipv4_addr, instant) => Some(format!(
                    "verifying {ipv4_addr} ({}s)",
                    instant.elapsed().as_secs()
                )),
            },
        })
    }
}

#[tauri::command]
async fn create_network(name: String, password: String) -> Result<MyInfo, String> {
    let mut guard = NETWORK.lock().await;
    *guard = None; // drop previous router if any

    let network = Network::new(&name, &password)
        .await
        .map_err(|e| e.to_string())?;
    info!(
        "Joined network with endpoint id {}",
        network
            .get_router_handle()
            .await
            .map_err(|e| e.to_string())?
            .get_node_id()
            .await
            .map_err(|e| e.to_string())?
    );

    *guard = Some(network.clone());

    MyInfo::from_network(&network)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug, Serialize)]
pub struct ConnectionState {
    pub peers: usize,
    pub ip: Option<String>,
    pub raw_ip_state: String,
}

#[tauri::command]
async fn connection_state() -> Result<ConnectionState, String> {
    let guard = NETWORK.lock().await;
    if let Some(network) = guard.as_ref() {
        // peer count
        let peers = network.get_peers().await.map_err(|e| e.to_string())?;

        let router = network
            .get_router_handle()
            .await
            .map_err(|e| e.to_string())?;
        let ip_state = router.get_ip_state().await.map_err(|e| e.to_string())?;
        let (ip, raw_ip_state) = match ip_state {
            iroh_lan::RouterIp::NoIp => (None, "NoIp".to_string()),
            iroh_lan::RouterIp::AquiringIp(candidate, _) => (
                Some(format!("acquiring {}...", candidate.ip)),
                "AquiringIp".to_string(),
            ),
            iroh_lan::RouterIp::AssignedIp(addr) => {
                (Some(addr.to_string()), "AssignedIp".to_string())
            }
            iroh_lan::RouterIp::VerifyingIp(addr, instant) => (
                Some(format!(
                    "verifying {addr} ({}s)",
                    instant.elapsed().as_secs()
                )),
                "VerifyingIp".to_string(),
            ),
        };

        Ok(ConnectionState {
            peers: peers.len(),
            ip,
            raw_ip_state,
        })
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn my_info() -> Result<MyInfo, String> {
    let guard = NETWORK.lock().await;
    if let Some(network) = guard.as_ref() {
        MyInfo::from_network(network)
            .await
            .map_err(|e| e.to_string())
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn list_peers() -> Result<Vec<PeerInfo>, String> {
    let guard = NETWORK.lock().await;
    if let Some(network) = guard.as_ref() {
        let direct_handle = network
            .get_direct_handle()
            .await
            .map_err(|e| e.to_string())?;
        let peers = network.get_peers().await.map_err(|e| e.to_string())?;
        let mut peer_infos = vec![];
        for peer in peers {
            let status = direct_handle
                .get_peer_state(peer.0)
                .await
                .map_err(|e| e.to_string())
                .unwrap_or(iroh_lan::ConnState::Disconnected);
            peer_infos.push(PeerInfo {
                node_id: peer.0.to_string(),
                ip: match peer.1 {
                    Some(ip) => ip.to_string(),
                    None => "unknown".to_string(),
                },
                status: match status {
                    iroh_lan::ConnState::Connecting => Status::Disconnected,
                    iroh_lan::ConnState::Open => Status::Active,
                    iroh_lan::ConnState::Disconnected => Status::Disconnected,
                    iroh_lan::ConnState::Closed => Status::Disconnected,
                    iroh_lan::ConnState::ClosedAndStopped => Status::Disconnected,
                },
            });
        }

        peer_infos.sort_by_key(|p| p.ip.clone());

        Ok(peer_infos)
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn close(window: tauri::Window) -> Result<(), String> {
    let mut guard = NETWORK.lock().await;
    if let Some(network) = guard.as_mut() {
        network.close().await.map_err(|e| e.to_string())?;
    }
    *guard = None; // drop

    window.close().map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_log::Builder::default()
                .filter(|metadata| {
                    matches!(
                        metadata.target(),
                        "iroh_lan::connection" | "iroh_lan::direct_connect" | "iroh_lan::router"
                    ) && metadata.level() <= log::Level::Debug
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            my_info,
            list_peers,
            connection_state,
            close,
            create_network,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
