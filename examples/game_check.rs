use rand::RngCore;
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{self, Instant};

const TICK_RATE: u64 = 50; // 50ms = 20 ticks per second (typical for games like Minecraft/Minetest)
const PACKET_SIZE: usize = 1200;
const DISCONNECT_TIMEOUT_SEC: u64 = 60; // Fail if no packets for 60s
const RECONNECT_LOG_THRESHOLD_SEC: u64 = 2;
const DONE_TAG: &[u8] = b"GAME_DONE";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: game_check <server|client> [bind_addr] [target_addr] [duration_minutes]");
        return Ok(());
    }

    let mode = &args[1];
    let bind_addr = args.get(2).unwrap_or(&"0.0.0.0:30000".to_string()).clone();
    let duration_minutes: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(5);
    let run_duration = Duration::from_secs(duration_minutes * 60);

    match mode.as_str() {
        "server" => run_server(&bind_addr, run_duration).await?,
        "client" => {
            let target_addr = args.get(3).expect("Client needs target address");
            run_client(&bind_addr, target_addr, run_duration).await?
        }
        _ => eprintln!("Invalid mode. Use server or client"),
    }

    Ok(())
}

async fn run_server(
    bind_addr: &str,
    _duration: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    println!("[Server] Listening on {}", bind_addr);
    println!("[Server] Waiting for client completion (duration handled by client).");

    let clients = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let recv_count = Arc::new(AtomicU64::new(0));
    let send_count = Arc::new(AtomicU64::new(0));

    let socket_recv = socket.clone();
    let clients_recv = clients.clone();
    let recv_count_recv = recv_count.clone();
    let active = Arc::new(AtomicBool::new(true));
    let active_recv = active.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while active_recv.load(Ordering::Relaxed) {
            if let Ok(Ok((len, addr))) =
                time::timeout(Duration::from_secs(1), socket_recv.recv_from(&mut buf)).await
                && len > 0
            {
                let mut guard = clients_recv.lock().await;
                if !guard.contains_key(&addr) {
                    println!("[Server] New Player Connected: {}", addr);
                }
                let entry = guard.entry(addr).or_insert((Instant::now(), false));
                entry.0 = Instant::now();
                recv_count_recv.fetch_add(1, Ordering::Relaxed);
                if len >= DONE_TAG.len() && &buf[..DONE_TAG.len()] == DONE_TAG {
                    entry.1 = true;
                    println!("[Server] Player {} finished test.", addr);
                }
            }
        }
    });

    let clients_log = clients.clone();
    let recv_count_log = recv_count.clone();
    let send_count_log = send_count.clone();
    let active_log = active.clone();
    tokio::spawn(async move {
        let mut log_interval = time::interval(Duration::from_secs(5));
        let mut last_recv = 0u64;
        let mut last_send = 0u64;
        while active_log.load(Ordering::Relaxed) {
            log_interval.tick().await;
            let guard = clients_log.lock().await;
            let recv_now = recv_count_log.load(Ordering::Relaxed);
            let send_now = send_count_log.load(Ordering::Relaxed);
            let recv_delta = recv_now.saturating_sub(last_recv);
            let send_delta = send_now.saturating_sub(last_send);
            last_recv = recv_now;
            last_send = send_now;
            println!(
                "[Server] Active clients: {} | recv/s: {} | send/s: {}",
                guard.len(),
                recv_delta / 5,
                send_delta / 5
            );
        }
    });

    let mut interval = time::interval(Duration::from_millis(TICK_RATE));
    let mut packet_data = vec![0u8; PACKET_SIZE];
    for (i, byte) in packet_data.iter_mut().enumerate() {
        *byte = (i % 255) as u8;
    }

    while active.load(Ordering::Relaxed) {
        interval.tick().await;

        let mut guard = clients.lock().await;
        // Remove dead clients (no packet in 20s)
        let now = Instant::now();
        guard.retain(|addr, (last_seen, _done)| {
            if now.duration_since(*last_seen) > Duration::from_secs(20) {
                println!("[Server] Player {} timed out", addr);
                false
            } else {
                true
            }
        });

        let any_done = guard.values().any(|(_, done)| *done);
        if any_done {
            println!("[Server] Client finished. Shutting down.");
            active.store(false, Ordering::Relaxed);
            break;
        }

        // Broadcast World State to all connected clients
        for (addr, _) in guard.iter() {
            let _ = socket.send_to(&packet_data, addr).await;
            send_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    Ok(())
}

async fn run_client(
    bind_addr: &str,
    target_addr: &str,
    duration: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    println!("[Client] Bound to {}", bind_addr);
    println!("[Client] Connecting to Game Server at {}", target_addr);

    let last_packet = Arc::new(tokio::sync::Mutex::new(Instant::now()));
    let last_gap_ms = Arc::new(AtomicU64::new(0));
    let packet_count = Arc::new(AtomicU64::new(0));

    // Receiver Loop
    let socket_recv = socket.clone();
    let last_packet_recv = last_packet.clone();
    let last_gap_ms_recv = last_gap_ms.clone();
    let packet_count_recv = packet_count.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut last_gap_logged = false;
        loop {
            if let Ok((len, _)) = socket_recv.recv_from(&mut buf).await
                && len > 0
            {
                let now = Instant::now();
                let mut lock = last_packet_recv.lock().await;
                let gap = now.duration_since(*lock);
                *lock = now;
                packet_count_recv.fetch_add(1, Ordering::Relaxed);
                last_gap_ms_recv.store(gap.as_millis() as u64, Ordering::Relaxed);

                if gap > Duration::from_secs(RECONNECT_LOG_THRESHOLD_SEC) {
                    println!("[Client] RECONNECTED (gap {:.2}s)", gap.as_secs_f32());
                    last_gap_logged = true;
                } else if last_gap_logged {
                    println!("[Client] Connection stable again.");
                    last_gap_logged = false;
                }
            }
        }
    });

    // Game Client Loop
    let start_time = Instant::now();
    let mut interval = time::interval(Duration::from_millis(TICK_RATE));
    let mut input_data = vec![0u8; 200];
    rand::rng().fill_bytes(&mut input_data);

    println!("[Client] Game Loop Started (Duration: {:?})", duration);
    println!("[Client] CSV: time_s,packets_total,packets_per_sec,last_gap_s");

    let packet_count_log = packet_count.clone();
    let last_gap_ms_log = last_gap_ms.clone();
    let start_time_log = start_time;
    let active_log = Arc::new(AtomicBool::new(true));
    let active_log_clone = active_log.clone();
    tokio::spawn(async move {
        let mut stat_interval = time::interval(Duration::from_secs(1));
        let mut last_packets = 0u64;
        loop {
            stat_interval.tick().await;
            if !active_log_clone.load(Ordering::Relaxed) {
                break;
            }
            let total = packet_count_log.load(Ordering::Relaxed);
            let delta = total.saturating_sub(last_packets);
            last_packets = total;
            let gap_s = last_gap_ms_log.load(Ordering::Relaxed) as f32 / 1000.0;
            let time_s = start_time_log.elapsed().as_secs_f32();
            println!(
                "[Client] CSV: {:.0},{},{}, {:.2}",
                time_s, total, delta, gap_s
            );
        }
    });

    while start_time.elapsed() < duration {
        interval.tick().await;

        match socket.send_to(&input_data, target_addr).await {
            Ok(_) => {}
            Err(e) => eprintln!("[Client] Failed to send input: {}", e),
        }

        let time_since_last = {
            let lock = last_packet.lock().await;
            Instant::now().duration_since(*lock)
        };

        if time_since_last > Duration::from_secs(DISCONNECT_TIMEOUT_SEC) {
            eprintln!("\n[Client] CRITICAL FAILURE: Connection Lost!");
            eprintln!(
                "[Client] No packets received for {:?} (Timeout is {}s)",
                time_since_last, DISCONNECT_TIMEOUT_SEC
            );
            std::process::exit(1);
        }

        if start_time.elapsed().as_secs().is_multiple_of(10)
            && start_time.elapsed().subsec_millis() < 55
        {
            println!(
                "[Client] Stats: Time={:.0}s | Packets Received: {}",
                start_time.elapsed().as_secs_f32(),
                packet_count.load(Ordering::Relaxed)
            );
        }
    }

    active_log.store(false, Ordering::Relaxed);
    println!("[Client] Session Complete. SUCCESS.");
    println!(
        "[Client] Total Packets Received: {}",
        packet_count.load(Ordering::Relaxed)
    );

    for _ in 0..3 {
        let _ = socket.send_to(DONE_TAG, target_addr).await;
        time::sleep(Duration::from_millis(100)).await;
    }

    Ok(())
}
