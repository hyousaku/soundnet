mod audio;
mod config;
mod control;
mod discovery;
mod iface;
mod pipeline;
mod routing;
mod rt;
mod state;
mod tone;
mod transport;

use anyhow::{Context, Result};
use clap::Parser;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Package version plus the git commit stamped in by build.rs. "Is this
/// machine running the build I just deployed?" has cost this project more
/// time than any real bug, and `CARGO_PKG_VERSION` cannot answer it — it has
/// said 0.1.0 since the first commit. This makes `--version`, the startup
/// log line and the node list in the UI all answer it directly.
const BUILD_ID: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SOUNDNET_BUILD"), ")");

/// SoundNet — low-latency LAN audio engine.
#[derive(Debug, Parser)]
#[command(version = BUILD_ID, about)]
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

    // Lock audio buffers into RAM before anything else starts allocating.
    // Whole-process, once — see rt.rs for why this degrades gracefully
    // instead of failing startup on boxes without the MEMLOCK rlimit.
    rt::lock_memory();

    let cli = Cli::parse();

    // Check the library we actually loaded, not the headers we compiled
    // against — version.h itself notes the two "may be different when using
    // shared library", and on a machine with both a distro 0.3 and a
    // source-built 0.4 that is exactly what happens. Against 0.3 the config
    // structs are read at the wrong offsets, so nothing works and the errors
    // name fields that do not exist in this version. Failing here, once, with
    // the real numbers beats that by a wide margin.
    match roc_sys::check_runtime_version() {
        Ok((major, minor, patch)) => tracing::info!("libroc {major}.{minor}.{patch}"),
        Err(msg) => anyhow::bail!("{msg}\n\nSee the libroc section of packaging/README.md."),
    }

    let hostname = hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "localhost".to_string());

    let node_name = cli.name.clone().unwrap_or_else(|| hostname.clone());

    let bind_addr = cli.bind;

    let cfg_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_path);
    let mut cfg = config::Config::load_or_default(Some(&cfg_path))?;

    let advertise_ip = pick_advertise_ip(bind_addr.ip(), cfg.interface.as_deref());

    // Node identity must survive restarts (crash, `systemctl restart`, a
    // rebuild) or every peer keeps showing both the old and new instance
    // forever — nothing ever tells them the old id is gone. Generate one
    // only on first run, and save it back immediately so a crash on the
    // very next line still finds it on disk next time.
    let node_id = match cfg.node_id.clone() {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            cfg.node_id = Some(id.clone());
            if let Err(err) = cfg.save(&cfg_path) {
                tracing::warn!(
                    "failed to persist new node_id to {}: {err:#}",
                    cfg_path.display()
                );
            }
            id
        }
    };

    let state = state::EngineState::new(state::EngineIdentity {
        node_id,
        hostname: node_name,
        addr: std::sync::RwLock::new(advertise_ip),
        control_port: bind_addr.port(),
        audio_port: cli.audio_port,
        version: BUILD_ID.to_string(),
    });

    // Populate local ports (ALSA + tone) up front and remember them in state.
    audio::devices::refresh(&state)?;
    tone::register_default_tones(&state);

    // Remember where to persist changes.
    *state.config_path.write().await = Some(cfg_path.clone());
    *state.manual_hosts.write().await = cfg.manual_hosts.clone();
    // Keep the pinned name itself (not whether it currently resolved) so a
    // temporarily-missing NIC (unplugged, mid-rename) doesn't get silently
    // forgotten the next time something calls routing::persist() — only an
    // explicit operator action (or it resolving again) should change this.
    *state.selected_interface.write().await = cfg.interface.clone();
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
    // mDNS ServiceRemoved only fires on a graceful goodbye; a crash, SIGKILL,
    // or re-IP via DHCP just leaves the old peer record in state.peers
    // forever. Poll each known peer's HTTP endpoint so dead ones eventually
    // get pruned even without a goodbye packet.
    discovery::spawn_liveness_checker(state.clone());
    // Retries driven purely by peer (re-)discovery miss two cases: a route
    // whose peer never flaps again after the route dies, and one that never
    // had a working peer-independent cause (e.g. the audio device) fixed
    // except by operator action unrelated to mDNS. This sweep catches both.
    routing::spawn_route_supervisor(state.clone());

    // Control plane (HTTP + WebSocket + embedded web UI).
    let control_handle = tokio::spawn(control::serve(state.clone(), bind_addr));

    tracing::info!(
        "soundnet-engine {} listening on http://{}  (advertise ip {}, audio {})",
        BUILD_ID, bind_addr, advertise_ip, cli.audio_port
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

/// Choose which IP to advertise in mDNS records and to bind audio egress to.
/// Priority: an explicit non-wildcard `--bind` always wins (the operator
/// already told us exactly which address to use); otherwise the pinned
/// interface from Config, resolved to its current IP; otherwise the first
/// non-loopback IPv4 interface the OS lists (unchanged fallback behaviour
/// from before interface pinning existed).
///
/// A pinned name that doesn't currently resolve (renamed NIC, unplugged, or
/// a config copied from the other deployed machine) must not prevent
/// startup — fall back to automatic and say so.
fn pick_advertise_ip(bound: IpAddr, pinned_name: Option<&str>) -> IpAddr {
    if let IpAddr::V4(v4) = bound {
        if v4 != Ipv4Addr::UNSPECIFIED {
            return IpAddr::V4(v4);
        }
    }
    if let Some(name) = pinned_name {
        match iface::resolve(name) {
            Some(ip) => return ip,
            None => tracing::warn!(
                "configured interface {name:?} not found on this host; falling back to automatic selection"
            ),
        }
    }
    iface::first_non_loopback_ipv4().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}
