use serde::Deserialize;
use specta::Type;
use tauri_specta;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default().commands(tauri_specta::collect_commands![crate::connect_peer])
    .path("../src/lib/bindings.ts")
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

#[derive(Debug, Deserialize, Type)]
pub struct ConnectArgs {
    target: String,
}

#[derive(Debug, Type)]
pub struct PeerInfo {
    id: String,
    addr: String,
    reachable: bool,
}

#[tauri::command]
#[specta::specta]
async fn connect_peer(args: ConnectArgs) -> PeerInfo {
    PeerInfo {
        id: "abc123".into(),
        addr: args.target,
        reachable: true,
    }
}