//mod direct_connect;
//mod local_networking;
mod local_networking;
mod router;
mod direct_connect;
mod connection;
pub mod network;

//pub mod actor;
pub mod actor_impr;

pub use actor_impr as actor;

pub use local_networking::Tun;
pub use direct_connect::{Direct, DirectMessage};
pub use router::Router;