//mod direct_connect;
//mod local_networking;
mod local_networking;
mod router;
mod direct_connect;
mod connection;
pub mod network;


pub use local_networking::Tun;
pub use direct_connect::{Direct, DirectMessage};
pub use connection::ConnState;
pub use router::{Router, RouterIp, IpAssignment, IpCandidate};