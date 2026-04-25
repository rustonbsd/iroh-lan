use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {

    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    return iroh_lan::cli::run_cli().await

    /*
    let last_tokio_tick = Arc::new(AtomicU64::new(0));
    let tick_for_watchdog = last_tokio_tick.clone();

    std::thread::Builder::new()
        .name("watchdog".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let tokio_tick = tick_for_watchdog.load(Ordering::Relaxed);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let ago = if tokio_tick > 0 {
                    now.saturating_sub(tokio_tick)
                } else {
                    0
                };
                eprintln!(
                    "[WATCHDOG] {} | tokio_tick={}s_ago",
                    std::fs::read_to_string("/proc/self/status")
                        .unwrap_or_default()
                        .lines()
                        .find(|l| l.starts_with("Threads:"))
                        .unwrap_or("Threads: ?"),
                    ago,
                );

                // When freeze detected, capture full backtraces via GDB
                if ago > 10 {
                    eprintln!("\n[FREEZE DETECTED] tokio_tick={}s_ago\n", ago);
                    let pid = std::process::id();

                    // GDB output goes to a FILE (not pipe) to avoid deadlock:
                    // GDB ptrace-stops ALL threads including this watchdog.
                    // With pipe: GDB fills 64KB buffer, blocks on write, watchdog
                    // can't read (stopped) → deadlock. Files have no such limit.
                    let bt_path = "/app/results/gdb_bt.txt";
                    eprintln!("[FREEZE] Running GDB (output → {bt_path})...");

                    let gdb_result = std::fs::File::create(bt_path).and_then(|outfile| {
                        let errfile = outfile.try_clone()?;
                        std::process::Command::new("gdb")
                            .args([
                                "-batch",
                                "-ex", "thread apply all bt",
                                "-p", &pid.to_string(),
                            ])
                            .stdout(outfile)
                            .stderr(errfile)
                            .spawn()
                    });

                    match gdb_result {
                        Ok(mut child) => {
                            // GDB will ptrace-stop us, generate backtraces,
                            // write to file, detach, then we resume here.
                            match child.wait() {
                                Ok(s) => eprintln!("[FREEZE] GDB exited: {s}"),
                                Err(e) => eprintln!("[FREEZE] GDB wait err: {e}"),
                            }
                            // Print the captured backtraces
                            if let Ok(bt) = std::fs::read_to_string(bt_path) {
                                eprintln!("{bt}");
                            }
                        }
                        Err(e) => {
                            eprintln!("[FREEZE] GDB failed ({e}), falling back to /proc");
                            if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) {
                                for entry in entries.flatten() {
                                    let tid = entry.file_name();
                                    let tid = tid.to_string_lossy();
                                    let comm = std::fs::read_to_string(
                                        format!("/proc/{pid}/task/{tid}/comm")
                                    ).unwrap_or_default();
                                    let wchan = std::fs::read_to_string(
                                        format!("/proc/{pid}/task/{tid}/wchan")
                                    ).unwrap_or_default();
                                    let syscall = std::fs::read_to_string(
                                        format!("/proc/{pid}/task/{tid}/syscall")
                                    ).unwrap_or_default();
                                    eprintln!(
                                        "=== tid={tid} name={} wchan={} syscall={} ===",
                                        comm.trim(), wchan.trim(), syscall.trim()
                                    );
                                }
                            }
                        }
                    }

                    eprintln!("\n[FREEZE DUMP COMPLETE]");
                    std::process::exit(99);
                }
            }
        })
        .ok();

    std::panic::set_hook(Box::new(|info| {
        eprintln!("[PANIC] {}", info);
        eprintln!("[PANIC] {:?}", std::backtrace::Backtrace::force_capture());
    }));

    let tick_for_tokio = last_tokio_tick.clone();

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .thread_name("iroh-worker")
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let tick = tick_for_tokio;
            tokio::spawn(async move {
                loop {
                    tick.store(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                        Ordering::Relaxed,
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });
            iroh_lan::cli::run_cli().await
        })
        */
}
