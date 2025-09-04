import { invoke } from "@tauri-apps/api/core";
import type { ConnectArgs, PeerInfo } from "./bindings-types";

export function connectPeer(args: ConnectArgs) {
  return invoke<PeerInfo>("connect_peer", { args });
}

export type { ConnectArgs, PeerInfo };