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

type PeerInfo = { node_id: string; ip: string; status: string };
type MyInfo = { node_id: string; ip?: string | null; leader: boolean };

enum ViewState {
  Lobby = "lobby",
  Network = "network",
}

export default function App() {
  const [view, setView] = useState<ViewState>(ViewState.Lobby);
  const [mode, setMode] = useState<"create" | "join">("create");
  const [networkName, setNetworkName] = useState("");
  const [password, setPassword] = useState("");
  const [loading, setLoading] = useState(false);
  const [myInfo, setMyInfo] = useState<MyInfo | null>(null);
  const [peers, setPeers] = useState<PeerInfo[]>([]);
  const [peerRefreshTick, setPeerRefreshTick] = useState(0);

  const connect = async () => {
    if (!networkName) return;
    setLoading(true);
    try {
      const cmd = mode === "create" ? "create_network" : "join_network";
      const res: MyInfo = await invoke(cmd, { name: networkName, password });
      setMyInfo(res);
      setView(ViewState.Network);
      toast.success(`${mode === "create" ? "Created" : "Joined"} network`);
      // After join, poll a few times for IP allocation if missing
      if (!res.ip) {
        for (let i = 0; i < 10; i++) {
          await new Promise((r) => setTimeout(r, 1000));
            try {
              const info: MyInfo = await invoke("my_info");
              if (info.ip) { setMyInfo(info); break; }
            } catch {}
        }
      }
    } catch (e: any) {
      toast.error(String(e));
    } finally {
      setLoading(false);
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

  const disconnect = async () => {
    await invoke("disconnect");
    setPeers([]);
    setMyInfo(null);
    setView(ViewState.Lobby);
    setNetworkName("");
    setPassword("");
    toast("Disconnected");
  };

  return (
    <div className="dark min-h-screen w-full bg-background text-foreground antialiased">
      <Toaster position="top-right" richColors />
      <div className="mx-auto w-full max-w-4xl p-6 flex flex-col gap-8">
        <header className="flex items-center justify-between">
          <div className="flex flex-col">
            <h1 className="font-semibold tracking-tight text-xl">iroh-lan</h1>
            <span className="text-xs text-muted-foreground">Ephemeral overlay network</span>
          </div>
          {view === ViewState.Network && myInfo && (
            <div className="flex items-center gap-4">
              <div className="flex flex-col gap-0 text-[10px] font-mono leading-tight">
                <span className="opacity-70">IP {myInfo.ip ?? "allocating…"}</span>
                <span className="opacity-50">ID {myInfo.node_id.slice(0, 18)}…</span>
                {myInfo.leader && <span className="text-primary">leader</span>}
              </div>
              <Button variant="outline" size="sm" onClick={disconnect}>Disconnect</Button>
            </div>
          )}
        </header>
        <Separator />
        {view === ViewState.Lobby && (
          <div className="grid gap-8">
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-lg">{mode === "create" ? "Create Network" : "Join Network"}</CardTitle>
                <CardDescription>{mode === "create" ? "Start a new overlay and become leader." : "Join an existing overlay by name."}</CardDescription>
              </CardHeader>
              <CardContent className="space-y-4 pt-2">
                <div className="flex gap-2">
                  <Button variant={mode === "create" ? "default" : "outline"} size="sm" onClick={() => setMode("create")}>Create</Button>
                  <Button variant={mode === "join" ? "default" : "outline"} size="sm" onClick={() => setMode("join")}>Join</Button>
                </div>
                <div className="grid gap-2">
                  <Label htmlFor="networkName">Network Name</Label>
                  <Input id="networkName" placeholder="lan-party" value={networkName} onChange={(e) => setNetworkName(e.target.value)} autoFocus />
                </div>
                <div className="grid gap-2">
                  <Label htmlFor="password">Password (optional)</Label>
                  <Input id="password" type="password" placeholder="••••••" value={password} onChange={(e) => setPassword(e.target.value)} />
                </div>
              </CardContent>
              <CardFooter className="justify-between">
                <p className="text-xs text-muted-foreground max-w-[60%] leading-relaxed">Node IDs rotate on restart. Share the network name (and password if set) with peers.</p>
                <Button disabled={!networkName || loading} onClick={connect}>
                  {loading ? (mode === "create" ? "Creating…" : "Joining…") : mode === "create" ? "Create" : "Join"}
                </Button>
              </CardFooter>
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
                  <Table>
                    <TableHeader>
                      <TableRow>
                        <TableHead className="w-[160px]">IP</TableHead>
                        <TableHead>Node ID</TableHead>
                        <TableHead className="w-[100px] text-right">Status</TableHead>
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
                          <TableCell className="font-mono text-xs">{p.ip}</TableCell>
                          <TableCell className="font-mono text-[10px] opacity-70">{p.node_id}</TableCell>
                          <TableCell className="text-right">
                            <Badge variant="outline" className={cn("text-[10px]", p.status === "Active" && "border-emerald-500/40 text-emerald-400 bg-emerald-500/10")}>{p.status}</Badge>
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
