// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::process::exit;

#[tokio::main]
async fn main() {
    if !self_runas::is_elevated() {
        let _ = self_runas::admin();
        exit(0);
    }

    if std::env::args_os().len() > 1 && iroh_lan::cli::run_cli().await.is_ok() {
        return;
    }

    iroh_lan_lib::run()
}
