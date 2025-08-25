use std::collections::HashSet;

use iroh::NodeId;

#[derive(Debug)]
pub struct Direct {
    inner: DirectInner,
}

#[derive(Debug)]
struct DirectInner {
    node_id_connect: HashSet<NodeId,DirectConnect>,
}

struct DirectConnect {
    remote_node_id: NodeId,
    router_requester: tokio::sync::mpsc::Sender<RouterRequest>,
}

impl Direct {
    pub fn new() -> Self {


        Self {}
    }
}