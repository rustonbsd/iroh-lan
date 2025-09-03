//mod direct_connect;
//mod local_networking;
mod local_networking;
mod router;
mod direct_connect;
mod connection;

pub mod actor;


use std::time::SystemTime;

pub use local_networking::Tun;
pub use direct_connect::{Direct, DirectMessage};
pub use router::Router;