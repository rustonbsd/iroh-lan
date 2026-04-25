import { useEffect, useState, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "./components/ui/button";
import { Input } from "./components/ui/input";
import { Card, CardContent, CardHeader, CardTitle, CardDescription, CardFooter } from "./components/ui/card";
import { Label } from "./components/ui/label";
import { Badge } from "./components/ui/badge";
import { Table, TableHeader, TableBody, TableRow, TableHead, TableCell } from "./components/ui/table";
import { ScrollArea } from "./components/ui/scroll-area";
import { Separator } from "./components/ui/separator";
import { Toaster } from "./components/ui/sonner";
import { toast } from "sonner";
import { cn } from "./lib/utils";

type PeerStatus = "Active" | "Idle" | "Pending" | "Disconnected" | string;
type PeerInfo = { node_id: string; ip: string; status: PeerStatus };
type MyInfo = { node_id: string; ip?: string | null };

enum ViewState {
  Lobby = "lobby",
  Connecting = "connecting",
  Network = "network",
}

type ConnectionState = { peers: number; ip: string | null; raw_ip_state: string };

export default function App() {
  const [view, setView] = useState<ViewState>(ViewState.Lobby);
  const [networkName, setNetworkName] = useState("");
  const [password, setPassword] = useState("");
  const [loading, setLoading] = useState(false);
  const [myInfo, setMyInfo] = useState<MyInfo | null>(null);
  const [peers, setPeers] = useState<PeerInfo[]>([]);
  const [peerRefreshTick, setPeerRefreshTick] = useState(0);
  const [connectingMessages, setConnectingMessages] = useState<string[]>([]);
  const pushMessage = useCallback((m: string) => {
    setConnectingMessages((prev) => (prev.includes(m) ? prev : [...prev, m]));
  }, []);

  const connect = async () => {
    if (!networkName) return;
    setLoading(true);
    setConnectingMessages([]);
    pushMessage("Looking for at least one peer to connect to…");
    // Enter Connecting view immediately because create_network can take a while
    setView(ViewState.Connecting);
    try {
      const res: MyInfo = await invoke("create_network", { name: networkName, password });
      setMyInfo(res);
      toast.success("Network started (establishing IP)");
    } catch (e: any) {
      toast.error(String(e));
      setLoading(false);
      setView(ViewState.Lobby);
    }
  };

  const fetchPeers = useCallback(async () => {
    try {
      const list: PeerInfo[] = await invoke("list_peers");
      setPeers(list);
      const info: MyInfo = await invoke("my_info");
      setMyInfo(info);
    } catch {}
  }, []);

  useEffect(() => {
    if (view === ViewState.Network) {
      fetchPeers();
      const t = setInterval(() => setPeerRefreshTick((t) => t + 1), 4000);
      return () => clearInterval(t);
    }
  }, [view, fetchPeers]);

  useEffect(() => { if (view === ViewState.Network) fetchPeers(); }, [peerRefreshTick, view, fetchPeers]);

  // Poll connection_state while in Connecting
  useEffect(() => {
    if (view !== ViewState.Connecting) return;
    let cancelled = false;
    let lastPeers = -1;
    let lastRawState = "";
    const interval = setInterval(async () => {
      try {
        const state: ConnectionState = await invoke("connection_state");
        if (cancelled) return;
        if (state.peers !== lastPeers) {
          lastPeers = state.peers;
          if (state.peers === 0) {
            pushMessage("Looking for at least one peer to connect to…");
          } else {
            pushMessage(`Found ${state.peers} peer record${state.peers === 1 ? "" : "s"}`);
          }
        }
        if (state.raw_ip_state !== lastRawState) {
          lastRawState = state.raw_ip_state;
          if (state.raw_ip_state === "NoIp") pushMessage("No IP yet");
          if (state.raw_ip_state === "AquiringIp") pushMessage("Acquiring IP…");
          if (state.raw_ip_state === "VerifyingIp") pushMessage("Verifying IP…");
          if (state.raw_ip_state === "AssignedIp") pushMessage("Acquired IP");
        }
        if (state.ip && state.raw_ip_state === "AssignedIp") {
          // fetch full info and transition
            try {
              const info: MyInfo = await invoke("my_info");
              setMyInfo(info);
            } catch {}
          setView(ViewState.Network);
          setLoading(false);
        }
      } catch (e) {
        // swallow errors while connecting
      }
    }, 1000);
    return () => { cancelled = true; clearInterval(interval); };
  }, [view, pushMessage]);

  const close = async () => {
    await invoke("close");
    setPeers([]);
    setMyInfo(null);
    setView(ViewState.Lobby);
    setNetworkName("");
    setPassword("");
    toast("Disconnected");
  };

  return (
    <div className="dark min-h-screen w-full bg-background text-foreground antialiased">
      <Toaster
        position="bottom-right"
        richColors
        closeButton
        offset={8}
        gap={6}
        toastOptions={{
          duration: 1200,
          classNames: {
            toast: "px-2 py-1",
            title: "text-xs",
            description: "text-[11px]",
          },
        }}
      />
      <div className="mx-auto w-full max-w-4xl p-6 flex flex-col gap-8">
        <header className="flex items-center justify-between">
          <div className="flex flex-col">
            <h1 className="font-semibold tracking-tight text-xl">iroh-lan</h1>
            {view === ViewState.Network ? (
              <div className="flex flex-wrap items-center gap-3 text-[11px] text-muted-foreground">
                <div className="flex items-center gap-1">
                  <span>Network:</span>
                  <span
                    className="font-mono cursor-pointer hover:underline hover:text-foreground truncate max-w-[200px]"
                    title={networkName}
                    role="button"
                    onClick={() => {
                      if (!networkName) return;
                      navigator.clipboard.writeText(networkName);
                      toast.success("Copied network name");
                    }}
                  >
                    {networkName || "unknown"}
                  </span>
                </div>
                <div className="flex items-center gap-1">
                  <span>Password:</span>
                  <span
                    className={cn(
                      "font-mono truncate max-w-[200px]",
                      password && "cursor-pointer hover:underline hover:text-foreground"
                    )}
                    title={password ? "Click to copy password" : undefined}
                    role={password ? "button" : undefined}
                    onClick={() => {
                      if (!password) return;
                      navigator.clipboard.writeText(password);
                      toast.success("Copied password");
                    }}
                  >
                    {password ? "••••••" : "no password set"}
                  </span>
                </div>
              </div>
            ) : (
              <span className="text-xs text-muted-foreground">have a lan party with iroh</span>
            )}
          </div>
          {view === ViewState.Network && myInfo && (
            <div className="flex items-center gap-4">
              <div className="flex flex-col gap-1 text-[10px] font-mono leading-tight">
                <div className="flex items-center gap-1">
                  <span className="opacity-70">IP {myInfo.ip ?? "allocating…"}</span>
                  {myInfo.ip && (
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-5 px-2 text-[10px]"
                      onClick={() => {
                        navigator.clipboard.writeText(myInfo.ip!);
                        toast.success("Copied IP");
                      }}
                    >
                      copy
                    </Button>
                  )}
                </div>
                <span className="opacity-50 max-w-[200px] truncate inline-block align-bottom" title={myInfo.node_id}>ID {myInfo.node_id}</span>
              </div>
              <Button variant="outline" size="sm" onClick={close}>Close</Button>
            </div>
          )}
        </header>
        <Separator />
        {view === ViewState.Lobby && (
          <div className="grid gap-8">
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-lg">Join Network</CardTitle>
                <CardDescription>Participate or create an overlay network.</CardDescription>
              </CardHeader>
              <CardContent className="space-y-4 pt-2">
                <div className="grid gap-2">
                  <Label htmlFor="networkName">Network Name</Label>
                  <Input id="networkName" placeholder="lan-party" value={networkName} onChange={(e) => setNetworkName(e.target.value)} autoFocus />
                </div>
                <div className="grid gap-2">
                  <Label htmlFor="password">Password (optional)</Label>
                  <Input id="password" type="password" placeholder="••••••" value={password} onChange={(e) => setPassword(e.target.value)} />
                </div>
              </CardContent>
              <CardFooter className="flex-col items-start gap-3">
                <div className="flex w-full items-center justify-between">
                  <p className="text-xs text-muted-foreground max-w-[60%] leading-relaxed">Endpoint IDs rotate on restart. Share the network name (and password if set) with peers.</p>
                  <Button disabled={!networkName || loading} onClick={connect}>
                    {loading ? "Joining…" : "Join"}
                  </Button>
                </div>
                <p className="text-[11px] text-muted-foreground/70 leading-relaxed">
                  <span className="font-medium">Note:</span> All peers must use the same version of iroh-lan to guarantee compatibility.
                </p>
              </CardFooter>
            </Card>
          </div>
        )}
        {view === ViewState.Connecting && (
          <div className="grid gap-8">
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-lg">Connecting…</CardTitle>
                <CardDescription>Establishing overlay session</CardDescription>
              </CardHeader>
              <CardContent className="space-y-4 pt-2">
                <div className="flex flex-col gap-2 text-xs font-mono">
                  {connectingMessages.map((m, idx) => {
                    const isActive = idx === connectingMessages.length - 1;
                    return (
                      <div key={m} className="flex items-center gap-2">
                        <span
                          className={cn(
                            "w-2 h-2 rounded-full",
                            isActive ? "dot-pulse-green" : "bg-muted-foreground/40"
                          )}
                        />
                        <span>{m}</span>
                      </div>
                    );
                  })}
                  {connectingMessages.length === 0 && <span className="opacity-60">Starting…</span>}
                </div>
              </CardContent>
            </Card>
          </div>
        )}
        {view === ViewState.Network && (
          <div className="flex flex-col gap-6">
            <div className="flex items-center justify-between">
              <h2 className="text-sm font-medium tracking-wide uppercase text-muted-foreground">Peers</h2>
              <div className="flex gap-2">
                <Button variant="outline" size="sm" onClick={fetchPeers}>Refresh</Button>
              </div>
            </div>
            <Card className="p-0">
              <CardContent className="p-0">
                <ScrollArea className="max-h-[420px]">
                  <Table className="table-fixed">
                    <TableHeader>
                      <TableRow>
                        <TableHead className="w-[180px]">IP</TableHead>
                        <TableHead className="w-[calc(100%-360px)]">Endpoint ID</TableHead>
                        <TableHead className="w-[180px] text-right">Actions / Status</TableHead>
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {peers.length === 0 && (
                        <TableRow>
                          <TableCell colSpan={3} className="text-center text-xs text-muted-foreground py-6">No peers yet.</TableCell>
                        </TableRow>
                      )}
                      {peers.map((p) => (
                        <TableRow key={p.node_id + p.ip}>
                          <TableCell className="font-mono text-xs flex items-center gap-2">
                            <span>{p.ip}</span>
                            <Button
                              variant="outline"
                              size="sm"
                              className="h-6 px-2 text-[10px]"
                              onClick={() => {
                                navigator.clipboard.writeText(p.ip);
                                toast.success("Copied IP");
                              }}
                            >
                              copy
                            </Button>
                          </TableCell>
                          <TableCell className="font-mono text-[10px] opacity-70 truncate max-w-[1px]" title={p.node_id}>{p.node_id}</TableCell>
                          <TableCell className="text-right flex items-center justify-end gap-2">
                            <Badge
                              variant="outline"
                              className={cn(
                                "text-[10px]",
                                p.status === "Active" && "border-emerald-500/40 text-emerald-400 bg-emerald-500/10"
                              )}
                            >
                              {p.status}
                            </Badge>
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                </ScrollArea>
              </CardContent>
              <CardFooter className="justify-end text-[10px] text-muted-foreground">Auto-refreshing every 4s</CardFooter>
            </Card>
          </div>
        )}
      </div>
    </div>
  );
}
