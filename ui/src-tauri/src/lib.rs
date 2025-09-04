use std::sync::Arc;

use once_cell::sync::Lazy;
use serde::Serialize;
use tokio::sync::Mutex;

// Re-export lib so main.rs can call run()
// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

static ROUTER: Lazy<Mutex<Option<iroh_lan::Router>>> = Lazy::new(|| Mutex::new(None));

#[derive(Debug, Serialize)]
pub struct PeerInfo {
    pub node_id: String,
    pub ip: String,
    pub status: String, // currently always Active; placeholder for future states
}

#[derive(Debug, Serialize)]
pub struct MyInfo {
    pub node_id: String,
    pub ip: Option<String>,
    pub leader: bool,
}

fn node_id_to_string(id: &iroh::NodeId) -> String {
    format!("{}", id) // Display impl from iroh
}

#[tauri::command]
async fn create_network(name: String, _password: String) -> Result<MyInfo, String> {
    let mut guard = ROUTER.lock().await;
    *guard = None; // drop previous router (disconnect)

    let builder = iroh_lan::Router::builder().entry_name(&name).creator_mode().password(&_password);
    let router = builder.build().await.map_err(|e| e.to_string())?;
    let my_id = router.node_id();
    let state = router.get_state().await.map_err(|e| e.to_string())?;
    let ip = state.node_id_ip_dict.get(&my_id).map(|ip| ip.to_string());
    let leader = state.leader.map(|l| l == my_id).unwrap_or(false);
    *guard = Some(router);
    Ok(MyInfo {
        node_id: node_id_to_string(&my_id),
        ip,
        leader,
    })
}

#[tauri::command]
async fn join_network(name: String, _password: String) -> Result<MyInfo, String> {
    let mut guard = ROUTER.lock().await;
    *guard = None; // drop previous router if any

    let builder = iroh_lan::Router::builder().entry_name(&name).password(&_password);
    let router = builder.build().await.map_err(|e| e.to_string())?;
    let my_id = router.node_id();
    // Wait up to ~10s for leader to assign IP (poll every 1s)
    let mut ip: Option<String> = None;
    let mut leader = false;
    for _ in 0..10 {
        let state = router.get_state().await.map_err(|e| e.to_string())?;
        ip = state.node_id_ip_dict.get(&my_id).map(|ip| ip.to_string());
        leader = state.leader.map(|l| l == my_id).unwrap_or(false);
        if ip.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    *guard = Some(router);
    Ok(MyInfo {
        node_id: node_id_to_string(&my_id),
        ip,
        leader,
    })
}

#[tauri::command]
async fn my_info() -> Result<MyInfo, String> {
    let guard = ROUTER.lock().await;
    if let Some(router) = guard.as_ref() {
        let my_id = router.node_id();
        let state = router.get_state().await.map_err(|e| e.to_string())?;
        let ip = state.node_id_ip_dict.get(&my_id).map(|ip| ip.to_string());
        let leader = state.leader.map(|l| l == my_id).unwrap_or(false);
        Ok(MyInfo {
            node_id: node_id_to_string(&my_id),
            ip,
            leader,
        })
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn list_peers() -> Result<Vec<PeerInfo>, String> {
    let guard = ROUTER.lock().await;
    if let Some(router) = guard.as_ref() {
        let state = router.get_state().await.map_err(|e| e.to_string())?;
        let mut peers = Vec::new();
        for (node_id, ip) in state.node_id_ip_dict.iter() {
            peers.push(PeerInfo {
                node_id: node_id_to_string(node_id),
                ip: ip.to_string(),
                status: "Active".into(), // placeholder
            });
        }
        Ok(peers)
    } else {
        Err("not_connected".into())
    }
}

#[tauri::command]
async fn disconnect() -> Result<(), String> {
    let mut guard = ROUTER.lock().await;
    *guard = None; // drop
    Ok(())
}

// Kick (remove) a peer from the network. Placeholder: returns Err if router missing.
// The actual logic should be implemented in the main crate (e.g., a Router::kick_peer(NodeId) method).
#[tauri::command]
async fn kick_peer(node_id: String) -> Result<(), String> {
    let guard = ROUTER.lock().await;
    if let Some(_router) = guard.as_ref() {
        // TODO: wire into router once implemented. For now just Ok.
        // Example future code (pseudo):
        // let nid = node_id.parse::<iroh::NodeId>().map_err(|e| e.to_string())?;
        // _router.kick_peer(nid).await.map_err(|e| e.to_string())?;
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
            disconnect,
            kick_peer
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
