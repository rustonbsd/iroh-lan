export type ConnectArgs = {
  address: string;
  port?: number;
  // add other fields returned/expected by your Rust command
};

export type PeerInfo = {
  id: string;
  address: string;
  connected: boolean;
  // add other fields returned by the "connect_peer" command
};