use std::collections::HashMap;
use std::env;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time::{self, Instant};

#[derive(Debug, Clone)]
struct PeerStats {
    tx_count: u64,
    rx_count: u64,
    first_rx_at: Option<Instant>,
    last_rx_at: Option<Instant>,
    max_gap: Duration,
}

impl PeerStats {
    fn new() -> Self {
        Self {
            tx_count: 0,
            rx_count: 0,
            first_rx_at: None,
            last_rx_at: None,
            max_gap: Duration::from_secs(0),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 6 {
        eprintln!(
            "Usage: mesh_check <bind_addr> <self_ip> <peer_csv> <duration_sec> <max_reconnect_sec> [tick_ms]"
        );
        std::process::exit(2);
    }

    let bind_addr = &args[1];
    let self_ip: Ipv4Addr = args[2].parse()?;
    let peer_csv = &args[3];
    let duration_sec: u64 = args[4].parse()?;
    let max_reconnect_sec: u64 = args[5].parse()?;
    let tick_ms: u64 = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(100);

    let mut peers: Vec<Ipv4Addr> = peer_csv
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse::<Ipv4Addr>().ok())
        .filter(|ip| *ip != self_ip)
        .collect();
    peers.sort_unstable();
    peers.dedup();

    if peers.is_empty() {
        eprintln!(
            "MESH_CHECK: FAIL no peers to test against (self_ip={})",
            self_ip
        );
        std::process::exit(1);
    }

    let duration = Duration::from_secs(duration_sec);
    let max_reconnect = Duration::from_secs(max_reconnect_sec);

    println!(
        "MESH_CHECK: start self_ip={} bind={} peers={:?} duration={}s max_reconnect={}s tick={}ms",
        self_ip, bind_addr, peers, duration_sec, max_reconnect_sec, tick_ms
    );

    let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    let stats: Arc<Mutex<HashMap<Ipv4Addr, PeerStats>>> = Arc::new(Mutex::new(
        peers
            .iter()
            .copied()
            .map(|ip| (ip, PeerStats::new()))
            .collect(),
    ));

    let start = Instant::now();
    let mut send_tick = time::interval(Duration::from_millis(tick_ms));
    send_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let mut stat_tick = time::interval(Duration::from_secs(1));
    stat_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let mut recv_buf = vec![0u8; 2048];

    loop {
        if start.elapsed() >= duration {
            break;
        }

        tokio::select! {
            _ = send_tick.tick() => {
                let payload = format!("mesh:{}:{}", self_ip, start.elapsed().as_millis());
                let socket = Arc::clone(&socket);
                let stats = Arc::clone(&stats);
                let peers = peers.clone();
                tokio::spawn(async move {
                    for ip in &peers {
                        let addr = format!("{}:31000", ip);
                        if socket.send_to(payload.as_bytes(), &addr).await.is_ok() {
                            let mut guard = stats.lock().await;
                            if let Some(s) = guard.get_mut(ip) {
                                s.tx_count = s.tx_count.saturating_add(1);
                            }
                        } else {
                            eprintln!("MESH_CHECK: WARN failed to send to {}", addr);
                        }
                    }
                });

            }
            recv = socket.recv_from(&mut recv_buf) => {
                if let Ok((len, src)) = recv {
                    if len == 0 {
                        continue;
                    }
                    if let std::net::IpAddr::V4(src_v4) = src.ip() {
                        let now = Instant::now();
                        let mut guard = stats.lock().await;
                        if let Some(s) = guard.get_mut(&src_v4) {
                            if let Some(last) = s.last_rx_at {
                                let gap = now.duration_since(last);
                                if gap > s.max_gap {
                                    s.max_gap = gap;
                                }
                            }
                            if s.first_rx_at.is_none() {
                                s.first_rx_at = Some(now);
                            }
                            s.last_rx_at = Some(now);
                            s.rx_count = s.rx_count.saturating_add(1);
                        }
                    }
                }
            }
            _ = stat_tick.tick() => {
                let elapsed = start.elapsed();
                let guard = stats.lock().await;
                for (ip, s) in guard.iter() {
                    if s.first_rx_at.is_none() && elapsed > max_reconnect {
                        eprintln!(
                            "MESH_CHECK: FAIL peer={} never received packet within {}s",
                            ip,
                            max_reconnect_sec
                        );
                        std::process::exit(1);
                    }
                    if let Some(last) = s.last_rx_at
                        && Instant::now().duration_since(last) > max_reconnect
                    {
                        eprintln!(
                            "MESH_CHECK: FAIL peer={} no packets for > {}s",
                            ip,
                            max_reconnect_sec
                        );
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    let guard = stats.lock().await;
    let mut failed = false;
    for (ip, s) in guard.iter() {
        let first_rx_delay = s
            .first_rx_at
            .map(|t| t.duration_since(start).as_secs_f32())
            .unwrap_or(-1.0);
        println!(
            "MESH_CHECK: peer={} tx={} rx={} first_rx_delay_s={:.2} max_gap_s={:.2}",
            ip,
            s.tx_count,
            s.rx_count,
            first_rx_delay,
            s.max_gap.as_secs_f32()
        );

        if s.tx_count == 0 || s.rx_count == 0 {
            failed = true;
            eprintln!(
                "MESH_CHECK: FAIL peer={} tx={} rx={} (expected both > 0)",
                ip, s.tx_count, s.rx_count
            );
        }

        if let Some(first_rx_at) = s.first_rx_at {
            if first_rx_at.duration_since(start) > max_reconnect {
                failed = true;
                eprintln!(
                    "MESH_CHECK: FAIL peer={} first_rx exceeded {}s",
                    ip, max_reconnect_sec
                );
            }
        } else {
            failed = true;
            eprintln!("MESH_CHECK: FAIL peer={} never received any packet", ip);
        }

        if s.max_gap > max_reconnect {
            failed = true;
            eprintln!(
                "MESH_CHECK: FAIL peer={} max gap {:.2}s exceeded {}s",
                ip,
                s.max_gap.as_secs_f32(),
                max_reconnect_sec
            );
        }
    }

    if failed {
        eprintln!("MESH_CHECK: FAIL");
        std::process::exit(1);
    }

    println!("MESH_CHECK: PASS");
    Ok(())
}
