use std::net::SocketAddr;
use std::path::PathBuf;

use aghanim_relay::{IceServer, Relay, RelayConfig};
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// aghanim-network relay: WebRTC signaling, persistent sync peer and
/// WebSocket fallback.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// TOML config file. Flags override values from the file.
    #[arg(short, long, env = "AGHANIM_CONFIG")]
    config: Option<PathBuf>,
    /// Address to listen on [default: 0.0.0.0:8787]
    #[arg(short, long, env = "AGHANIM_LISTEN")]
    listen: Option<SocketAddr>,
    /// Data directory [default: ./data]
    #[arg(short, long, env = "AGHANIM_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// App id to serve (repeatable). Default: any app.
    #[arg(short, long = "app")]
    apps: Vec<String>,
    /// STUN/TURN URL to hand to browsers (repeatable). Replaces the default.
    #[arg(long = "ice")]
    ice: Vec<String>,
    /// Hand browsers no ICE servers at all (LAN or localhost only).
    #[arg(long, conflicts_with = "ice")]
    no_ice: bool,
    /// Max sync frames per second per connection.
    #[arg(long)]
    max_frames_per_sec: Option<u32>,
    /// Print the effective config as TOML and exit.
    #[arg(long)]
    print_config: bool,
}

fn load(args: &Args) -> Result<RelayConfig, String> {
    let mut cfg = match &args.config {
        Some(p) => {
            let s = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            toml::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))?
        }
        None => RelayConfig::default(),
    };
    if let Some(l) = args.listen {
        cfg.listen = l;
    }
    if let Some(d) = &args.data_dir {
        cfg.data_dir = d.clone();
    }
    if !args.apps.is_empty() {
        cfg.apps = args.apps.clone();
    }
    if !args.ice.is_empty() {
        cfg.ice_servers = args
            .ice
            .iter()
            .map(|u| IceServer {
                urls: vec![u.clone()],
                username: None,
                credential: None,
            })
            .collect();
    }
    if args.no_ice {
        cfg.ice_servers.clear();
    }
    if let Some(n) = args.max_frames_per_sec {
        cfg.max_frames_per_sec = n;
    }
    for a in &cfg.apps {
        if !aghanim_relay::config::valid_app_id(a) {
            return Err(format!("invalid app id {a:?}"));
        }
    }
    Ok(cfg)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let cfg = match load(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    if args.print_config {
        print!("{}", toml::to_string_pretty(&cfg).unwrap());
        return;
    }
    let listener = match tokio::net::TcpListener::bind(cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: can't listen on {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    let relay = Relay::open(cfg.clone()).unwrap_or_else(|e| {
        eprintln!("error: can't open data dir {}: {e}", cfg.data_dir.display());
        std::process::exit(1);
    });
    tracing::info!(listen = %cfg.listen, data = %cfg.data_dir.display(), "aghanim-relay ready");
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = aghanim_relay::serve(listener, relay, shutdown).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
