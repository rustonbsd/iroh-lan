use rand::RngCore;
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{self, Instant};

const TICK_RATE: u64 = 50; // 50ms = 20 ticks per second (typical for games like Minecraft/Minetest)
const PACKET_SIZE: usize = 1200;
const DISCONNECT_TIMEOUT_SEC: u64 = 10; // Fail if no packets for 10s

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

async fn run_server(bind_addr: &str, duration: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    println!("[Server] Listening on {}", bind_addr);
    println!("[Server] Running for {:?}...", duration);

    let active = Arc::new(AtomicBool::new(true));
    let clients = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    let active_clone = active.clone();
    tokio::spawn(async move {
        time::sleep(duration).await;
        println!("[Server] Time limit reached. Shutting down.");
        active_clone.store(false, Ordering::Relaxed);
    });

    let socket_recv = socket.clone();
    let clients_recv = clients.clone();
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
                guard.insert(addr, Instant::now());
            }
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
        guard.retain(|addr, last_seen| {
            if now.duration_since(*last_seen) > Duration::from_secs(20) {
                println!("[Server] Player {} timed out", addr);
                false
            } else {
                true
            }
        });

        // Broadcast World State to all connected clients
        for (addr, _) in guard.iter() {
            let _ = socket.send_to(&packet_data, addr).await;
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
    let packet_count = Arc::new(AtomicU64::new(0));

    // Receiver Loop
    let socket_recv = socket.clone();
    let last_packet_recv = last_packet.clone();
    let packet_count_recv = packet_count.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            if let Ok((len, _)) = socket_recv.recv_from(&mut buf).await
                && len > 0
            {
                let mut lock = last_packet_recv.lock().await;
                *lock = Instant::now();
                packet_count_recv.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    // Game Client Loop
    let start_time = Instant::now();
    let mut interval = time::interval(Duration::from_millis(TICK_RATE));
    let mut input_data = vec![0u8; 200];
    rand::rng().fill_bytes(&mut input_data);

    println!("[Client] Game Loop Started (Duration: {:?})", duration);

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

    println!("[Client] Session Complete. SUCCESS.");
    println!(
        "[Client] Total Packets Received: {}",
        packet_count.load(Ordering::Relaxed)
    );

    Ok(())
}
