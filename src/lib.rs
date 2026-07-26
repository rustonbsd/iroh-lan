mod connection;
mod direct_connect;
mod tun;
mod network;
mod overlay;
mod state;
pub mod cli;


pub use network::Network;
pub use direct_connect::{Direct, DirectMessage};
pub use tun::Tun;
pub use overlay::Overlay;
pub use connection::InnerConnState;


pub(crate) fn current_time() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

pub(crate) fn build_ip(base_ip: u16, suffix: u16) -> std::net::Ipv4Addr {
    std::net::Ipv4Addr::from((base_ip as u32) << 16 | (suffix as u32))
}