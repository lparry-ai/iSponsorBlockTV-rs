//! Rust port of iSponsorBlockTV: skips SponsorBlock segments on YouTube TV apps.

mod api;
mod config;
mod device;
mod dial;
mod homeassistant;
mod lounge;
mod mqtt;
mod setup;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::api::ApiHelper;
use crate::config::Config;
use crate::device::DeviceListener;

/// Upstream sources this port follows; see PORTING.md.
pub const PORTED_FROM: &str = "iSponsorBlockTV 2.11.0 (005d248), pyytlounge 3.3.0";

#[derive(Parser)]
#[command(version, about = "SponsorBlock client for YouTube TV apps", long_version = concat!(
    env!("CARGO_PKG_VERSION"), " (port of iSponsorBlockTV 2.11.0 / 005d248, pyytlounge 3.3.0)"
))]
struct Cli {
    /// Data directory containing config.json
    #[arg(short, long, env = "iSPBTV_data_dir", global = true)]
    data: Option<PathBuf>,
    /// Debug logging (equivalent to RUST_LOG=debug)
    #[arg(long, global = true)]
    debug: bool,
    /// Legacy flag from the Python CLI (same as `setup`)
    #[arg(long, hide = true)]
    setup: bool,
    /// Legacy flag from the Python CLI (same as `setup`)
    #[arg(long = "setup-cli", hide = true)]
    setup_cli: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start skipping (default)
    Start,
    /// Interactive setup: pair devices and choose options
    #[command(alias = "setup-cli")]
    Setup,
    /// List YouTube devices on the network (without launching or pairing)
    Discover,
}

/// Use `ring` for TLS: it cross-compiles to every platform the Docker image targets.
pub fn init_tls() {
    // Errors only if a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn http_client(use_proxy: bool) -> Result<reqwest::Client> {
    init_tls();
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("iSponsorBlockTV-rs/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15));
    if !use_proxy {
        // Match the Python default (trust_env=False): ignore HTTP(S)_PROXY unless asked.
        b = b.no_proxy();
    }
    Ok(b.build()?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter = if cli.debug {
        "isponsorblocktv=debug".into()
    } else {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "isponsorblocktv=info".into())
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let data_dir = cli.data.unwrap_or_else(config::default_data_dir);
    if std::env::var_os("iSPBTV_docker").is_some() && !data_dir.exists() {
        anyhow::bail!(
            "Running in docker without mounting the data dir. Mount a volume at /app/data, see \
             https://github.com/dmunozv04/iSponsorBlockTV/wiki/Installation#Docker"
        );
    }
    let legacy_setup = (cli.setup || cli.setup_cli).then_some(Command::Setup);
    match cli.command.or(legacy_setup).unwrap_or(Command::Start) {
        Command::Setup => setup::run(&data_dir).await,
        Command::Discover => {
            let config = Config::load(&data_dir)?;
            let api = Arc::new(ApiHelper::new(&config, http_client(config.use_proxy)?));
            let found = dial::discover(api, false).await?;
            if found.is_empty() {
                println!("No devices advertised a YouTube screen id (try `setup` to pair).");
            }
            for d in found {
                println!("{}\t{}", d.screen_id, d.name);
            }
            Ok(())
        }
        Command::Start => start(&data_dir).await,
    }
}

async fn start(data_dir: &std::path::Path) -> Result<()> {
    let mut config = Config::load(data_dir)?;
    config.validate()?;
    tracing::debug!("{PORTED_FROM}");
    let http = http_client(config.use_proxy)?;
    let api = Arc::new(ApiHelper::new(&config, http.clone()));
    let notifier = mqtt::Notifier::start(&config);
    let devices: Vec<Arc<DeviceListener>> = config
        .devices
        .iter()
        .map(|d| DeviceListener::new(&config, d, api.clone(), http.clone(), notifier.clone()))
        .collect();
    let tasks: Vec<_> = devices
        .iter()
        .map(|d| tokio::spawn(d.clone().run()))
        .collect();

    shutdown_signal().await;
    println!("Cancelling tasks and exiting...");
    // Stop the device loops first so they don't publish "disconnected" on the way out.
    for t in &tasks {
        t.abort();
    }
    let disconnects = devices.iter().map(|d| {
        let lounge = d.lounge.clone();
        async move { tokio::time::timeout(Duration::from_secs(3), lounge.disconnect()).await }
    });
    futures_util::future::join_all(disconnects).await;
    if let Some(n) = notifier {
        n.stop().await;
    }
    println!("Exited");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
