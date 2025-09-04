// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use self_runas;

fn main() {
    if !self_runas::is_elevated() {
        let _ = self_runas::admin();
        return;
    }
    iroh_lan_lib::run()
}
