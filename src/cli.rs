use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use iroh::EndpointId;
use iroh_lan::{RouterIp, network::Network};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::{io, net::Ipv4Addr, time::Duration};
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Name of the network
    #[arg(short, long, default_value = "test1")]
    name: String,

    /// Password for the network
    #[arg(short, long, default_value = "password")]
    password: String,

    /// Disable the TUI and run in headless mode (logs only)
    #[arg(short = 'd', long = "no-display")]
    no_display: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    if args.no_display {
        use tracing_subscriber::{EnvFilter, fmt, prelude::*};
        tracing_subscriber::registry()
            .with(fmt::layer().with_thread_ids(true))
            .with(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("iroh_lan=trace,info")),
            )
            .init();

        run_headless(args.name, args.password).await?;
    } else {
        run_tui(args.name, args.password).await?;
    }

    Ok(())
}

async fn run_headless(name: String, password: String) -> Result<()> {
    let network = Network::new(&name, &password).await?;

    println!("Starting network '{}'...", name);

    loop {
        let state = network.get_router_state().await?;
        if let RouterIp::AssignedIp(ip) = state {
            println!("My IP is {}", ip);
            break;
        }
        sleep(std::time::Duration::from_millis(500)).await;
    }

    // Keep running
    tokio::signal::ctrl_c().await?;

    Ok(())
}

struct AppState {
    name: String,
    password: String,
    node_id: Option<EndpointId>,
    my_ip: String,
    peers: Vec<(EndpointId, Option<Ipv4Addr>)>,
    status: String,
}

impl AppState {
    fn new(name: String, password: String) -> Self {
        Self {
            name,
            password,
            node_id: None,
            my_ip: "Unknown".to_string(),
            peers: vec![],
            status: "Initializing...".to_string(),
        }
    }
}

async fn run_tui(name: String, password: String) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Use a guard to clean up terminal state on panic or early return
    struct CleanUp;
    impl Drop for CleanUp {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        }
    }
    let _clean_up = CleanUp;

    // Create App State
    let mut app_state = AppState::new(name.clone(), password.clone());

    // Initial draw to show something immediately
    terminal.draw(|f| ui(f, &app_state))?;

    // Initialize Network in background so TUI remains responsive
    let (tx_network, mut rx_network) = tokio::sync::mpsc::channel(1);
    let name_clone = name.clone();
    let password_clone = password.clone();

    tokio::spawn(async move {
        match Network::new(&name_clone, &password_clone).await {
            Ok(network) => {
                let _ = tx_network.send(Ok(network)).await;
            }
            Err(e) => {
                let _ = tx_network.send(Err(e)).await;
            }
        }
    });

    let mut network: Option<Network> = None;

    // Main loop
    let tick_rate = Duration::from_millis(100);
    let mut last_tick = std::time::Instant::now();

    loop {
        terminal.draw(|f| ui(f, &app_state))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if crossterm::event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && (key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            // Explicitly break loop
            break;
        }

        if last_tick.elapsed() >= tick_rate {
            // Check if network is ready
            if network.is_none()
                && let Ok(res) = rx_network.try_recv()
            {
                match res {
                    Ok(net) => {
                        network = Some(net);
                        app_state.status = "Network Started".to_string();
                    }
                    Err(e) => {
                        app_state.status = format!("Init Failed: {}", e);
                    }
                }
            }

            // Update state if network exists
            if let Some(net) = &network {
                update_state(net, &mut app_state).await?;
            }
            last_tick = std::time::Instant::now();
        }
    }

    Ok(())
}

async fn update_state(network: &Network, state: &mut AppState) -> Result<()> {
    // Node ID
    if state.node_id.is_none()
        && let Ok(id) = network.get_node_id().await
    {
        state.node_id = Some(id);
    }

    // Router State (IP and Status)
    match network.get_router_state().await {
        Ok(RouterIp::NoIp) => {
            state.status = "No IP".to_string();
            state.my_ip = "None".to_string();
        }
        Ok(RouterIp::AquiringIp(_, _)) => {
            state.status = "Acquiring IP...".to_string();
            state.my_ip = "Acquiring...".to_string();
        }
        Ok(RouterIp::VerifyingIp(_, _)) => {
            state.status = "Verifying IP...".to_string();
            state.my_ip = "Verifying...".to_string();
        }
        Ok(RouterIp::AssignedIp(ip)) => {
            state.status = "Connected".to_string();
            state.my_ip = ip.to_string();
        }
        Err(e) => {
            state.status = format!("Error: {}", e);
        }
    }

    // Peers
    match network.get_peers().await {
        Ok(peers) => {
            state.peers = peers;
        }
        Err(_) => {
            // keep old peers or clear?
        }
    }

    Ok(())
}

fn ui(f: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(3), // Header
                Constraint::Length(6), // Info
                Constraint::Min(0),    // Peers
                Constraint::Length(3), // Footer
            ]
            .as_ref(),
        )
        .split(f.area());

    // Header
    let title = Paragraph::new(format!("Iroh LAN - Network: {}", state.name))
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // Info
    let node_id_str = state
        .node_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "Loading...".to_string());

    let info_text = vec![
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled(
                &state.status,
                Style::default().fg(if state.status == "Connected" {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
        ]),
        Line::from(vec![
            Span::raw("My IP: "),
            Span::styled(
                &state.my_ip,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("Node ID: "),
            Span::styled(node_id_str, Style::default().fg(Color::Gray)),
        ]),
        Line::from(vec![
            Span::raw("Password: "),
            Span::styled(&state.password, Style::default().fg(Color::Red)), // Maybe mask this or make it red
        ]),
    ];

    let info = Paragraph::new(info_text)
        .block(Block::default().title("Info").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    f.render_widget(info, chunks[1]);

    // Peers
    let peers: Vec<ListItem> = state
        .peers
        .iter()
        .map(|(id, ip)| {
            let ip_str = ip
                .map(|i| i.to_string())
                .unwrap_or_else(|| "Unknown IP".to_string());
            let content = vec![Line::from(vec![
                Span::styled(format!("{:<20}", ip_str), Style::default().fg(Color::Green)),
                Span::raw(" "),
                Span::styled(id.to_string(), Style::default().fg(Color::Gray)),
            ])];
            ListItem::new(content)
        })
        .collect();

    let peers_list = List::new(peers)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Connected Peers"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(peers_list, chunks[2]);

    // Footer
    let footer = Paragraph::new("Press 'q' to quit")
        .style(Style::default().fg(Color::Gray))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[3]);
}
