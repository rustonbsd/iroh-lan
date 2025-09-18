use iroh_lan::network::Network;
use once_cell::sync::Lazy;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{self, info};

// Re-export lib so main.rs can call run()
// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

static NETWORK: Lazy<Mutex<Option<Network>>> = Lazy::new(|| {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_thread_ids(true)
        .init();

    Mutex::new(None)
});

#[derive(Debug, Serialize)]
pub enum Status {
    Active,
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
                    Some(format!("acquiring {}...", ip_candidate.ip.to_string()))
                }
                iroh_lan::RouterIp::AssignedIp(ipv4_addr) => Some(format!("{}", ipv4_addr)),
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
        "Created network with node ID {}",
        network
            .get_router_handle()
            .await
            .map_err(|e| e.to_string())?
            .get_node_id()
            .await
            .map_err(|e| e.to_string())?
    );

    *guard = Some(network.clone());

    Ok(MyInfo::from_network(&network)
        .await
        .map_err(|e| e.to_string())?)
}

#[tauri::command]
async fn join_network(name: String, password: String) -> Result<MyInfo, String> {
    create_network(name, password).await
}

#[tauri::command]
async fn my_info() -> Result<MyInfo, String> {
    let guard = NETWORK.lock().await;
    if let Some(network) = guard.as_ref() {
        MyInfo::from_network(&network)
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
        let mut peers = network.get_peers().await.map_err(|e| e.to_string())?
            .iter()
            .map(|(node_id, ip)| {
                PeerInfo {
                    node_id: node_id.to_string(),
                    ip: match ip {
                        Some(ip) => ip.to_string(),
                        None => "unknown".to_string(),
                    },
                    status: Status::Active, // placeholder;
                }
            })
            .collect::<Vec<_>>();

        peers.sort_by_key(|p| p.ip.clone());

        Ok(peers)
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn close() -> Result<(), String> {
    let mut guard = NETWORK.lock().await;
    if let Some(network) = guard.as_mut() {
        network.close().await.map_err(|e| e.to_string())?;
    }
    *guard = None; // drop
    std::process::exit(0);

    #[allow(unreachable_code)]
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            create_network,
            join_network,
            my_info,
            list_peers,
            close
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
