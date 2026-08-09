mod audio;
mod config;
mod control;
mod discovery;
mod routing;
mod state;
mod tone;
mod transport;

use anyhow::{Context, Result};
use clap::Parser;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// SoundNet — low-latency LAN audio engine.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// TCP address to bind the HTTP/WebSocket control plane to.
    /// Default is 0.0.0.0:7788 (reachable from any LAN peer).
    #[arg(long, default_value = "0.0.0.0:7788")]
    bind: SocketAddr,

    /// Node display name. Defaults to `hostname`.
    #[arg(long)]
    name: Option<String>,

    /// Path to config file. Defaults to $XDG_CONFIG_HOME/soundnet/config.toml.
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Announce & receive on this UDP base port (source uses base, repair base+1, control base+2).
    #[arg(long, default_value_t = 10001)]
    audio_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,soundnet_engine=debug")),
        )
        .init();

    let cli = Cli::parse();

    let hostname = hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "localhost".to_string());

    let node_name = cli.name.clone().unwrap_or_else(|| hostname.clone());
    let node_id = uuid::Uuid::new_v4().to_string();

    let bind_addr = cli.bind;
    let advertise_ip = pick_advertise_ip(bind_addr.ip()).context("pick advertise ip")?;

    let cfg_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_path);
    let cfg = config::Config::load_or_default(Some(&cfg_path))?;

    let state = state::EngineState::new(state::EngineIdentity {
        node_id,
        hostname: node_name,
        addr: advertise_ip,
        control_port: bind_addr.port(),
        audio_port: cli.audio_port,
        version: env!("CARGO_PKG_VERSION").to_string(),
    });

    // Populate local ports (ALSA + tone) up front and remember them in state.
    audio::devices::refresh(&state)?;
    tone::register_default_tones(&state);

    // Remember where to persist changes.
    *state.config_path.write().await = Some(cfg_path.clone());
    *state.manual_hosts.write().await = cfg.manual_hosts.clone();
    // Best-effort resolve of manual hosts (their state fetch happens in discovery::add_manual).
    for mh in &cfg.manual_hosts {
        discovery::probe_manual(state.clone(), mh.addr.clone(), mh.port);
    }

    // Restore persisted routes into state.routes. try_start will pick them up
    // once the referenced peers appear via mDNS (or the manual host probe).
    for route in cfg.routes.iter().cloned() {
        state.routes.insert(route.id.clone(), route.clone());
        // If it references only self, try to start it right away.
        if let Err(err) = routing::try_start(&state, &route).await {
            tracing::debug!("restore: route {} pending: {err:#}", route.id);
        }
    }

    // Kick off discovery + stats pump in the background.
    discovery::spawn(state.clone(), advertise_ip, bind_addr.port(), cli.audio_port);
    routing::spawn_stats_pump(state.clone());

    // Control plane (HTTP + WebSocket + embedded web UI).
    let control_handle = tokio::spawn(control::serve(state.clone(), bind_addr));

    tracing::info!(
        "soundnet-engine listening on http://{}  (advertise ip {}, audio {})",
        bind_addr, advertise_ip, cli.audio_port
    );

    // systemd sends SIGTERM on `stop`/`restart`, not SIGINT — without a
    // handler for it, the process gets killed before it can send an mDNS
    // "goodbye", leaving a ghost node in every peer's list until the stale
    // record's TTL finally expires. Listen for both.
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    #[cfg(unix)]
    let sigterm_recv = sigterm.recv();
    #[cfg(not(unix))]
    let sigterm_recv = std::future::pending::<Option<()>>();

    tokio::select! {
        r = control_handle => r??,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl-C received, shutting down");
        }
        _ = sigterm_recv => {
            tracing::info!("SIGTERM received, shutting down");
        }
    }

    if let Some(mdns) = state.mdns.write().await.take() {
        if let Ok(rx) = mdns.daemon.unregister(&mdns.fullname) {
            // Give the goodbye packet a moment to actually go out before we
            // exit; ignore the result either way — this is best-effort.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                let _ = tokio::task::spawn_blocking(move || rx.recv()).await;
            })
            .await;
        }
    }

    routing::shutdown_all(&state).await;
    Ok(())
}

/// Choose which IP to advertise in mDNS records. If the user bound to 0.0.0.0
/// we pick the first non-loopback IPv4 interface; otherwise we honour the bind.
fn pick_advertise_ip(bound: IpAddr) -> Result<IpAddr> {
    match bound {
        IpAddr::V4(v4) if v4 != Ipv4Addr::UNSPECIFIED => Ok(IpAddr::V4(v4)),
        _ => Ok(first_non_loopback_ipv4().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))),
    }
}

fn first_non_loopback_ipv4() -> Option<IpAddr> {
    // if-addrs (transitively pulled by mdns-sd) gives us portable enumeration.
    for iface in if_addrs::get_if_addrs().ok()? {
        let ip = iface.ip();
        if let IpAddr::V4(v4) = ip {
            if !v4.is_loopback() && !v4.is_unspecified() && !v4.is_link_local() {
                return Some(IpAddr::V4(v4));
            }
        }
    }
    None
}
