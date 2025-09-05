use std::ops::Deref;

use once_cell::sync::Lazy;
use serde::Serialize;
use tokio::sync::Mutex;

// Re-export lib so main.rs can call run()
// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

static ROUTER: Lazy<Mutex<Option<(iroh_lan::Router, tokio::task::JoinHandle<()>)>>> = Lazy::new(|| Mutex::new(None));

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
    pub leader: bool,
}

impl MyInfo {
    pub async fn from_router(router: &iroh_lan::Router) -> anyhow::Result<Self> {
        Ok(Self {
            node_id: router.my_node_id().to_string(),
            ip: router.my_ip().await.map(|ip| ip.to_string()),
            leader: router.get_leader().await?.map(|l| l == router.my_node_id()).unwrap_or(false),
        })
    }
}

#[tauri::command]
async fn create_network(name: String, password: String) -> Result<MyInfo, String> {
    let mut guard = ROUTER.lock().await;
    *guard = None; // drop previous router if any

    let (router, _handle) = iroh_lan::standalone::run(&name, &password, true).await.map_err(|e| e.to_string())?;
    println!("Created network with node ID {}", router.my_node_id());
    *guard = Some((router.clone(), _handle));
    Ok(MyInfo::from_router(&router).await.map_err(|e| e.to_string())?)
}

#[tauri::command]
async fn join_network(name: String, password: String) -> Result<MyInfo, String> {
    let mut guard = ROUTER.lock().await;
    *guard = None; // drop previous router if any

    let (router, _handle) = iroh_lan::standalone::run(&name, &password, false).await.map_err(|e| e.to_string())?;
    println!("Joined network with node ID {}", router.my_node_id());
    
    *guard = Some((router.clone(), _handle));
    Ok(MyInfo::from_router(&router).await.map_err(|e| e.to_string())?)
}

#[tauri::command]
async fn my_info() -> Result<MyInfo, String> {
    let guard = ROUTER.lock().await;
    if let Some((router,_)) = guard.as_ref() {
        MyInfo::from_router(&router).await.map_err(|e| e.to_string())
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn list_peers() -> Result<Vec<PeerInfo>, String> {
    let guard = ROUTER.lock().await;
    if let Some((router,_)) = guard.as_ref() {
        let state = router.get_state().await.map_err(|e| e.to_string())?;
        let mut peers = state.node_id_ip_dict.iter().map(|(node_id,ip)| {
            PeerInfo {
                node_id: node_id.to_string(),
                ip: ip.to_string(),
                status: Status::Active, // placeholder;
            }
        }).collect::<Vec<_>>();

        peers.sort_by_key(|p| p.ip.clone());

        Ok(peers)
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn close() -> Result<(), String> {
    let mut guard = ROUTER.lock().await;
    if let Some((router,_)) = guard.as_mut() {
        router.close().await;
    }
    *guard = None; // drop
    std::process::exit(0);

    #[allow(unreachable_code)]
    Ok(())
}

#[tauri::command]
async fn kick_peer(node_id: String) -> Result<(), String> {
    let guard = ROUTER.lock().await;
    if let Some((router,_)) = guard.as_ref() {
        
        let nid = node_id.parse::<iroh::NodeId>().map_err(|e| e.to_string())?;
        router.direct.kick_peer(nid).await.map_err(|e| e.to_string())?;
        Ok(())
    } else {
        Err("not_connected".into())
    }
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
            close,
            kick_peer
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

