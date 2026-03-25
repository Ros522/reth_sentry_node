//! Lightweight Ethereum Sentry Node
//!
//! Connects to the Ethereum P2P network to collect mempool transactions,
//! performs stateless validation, and forwards valid transactions to
//! configured backend nodes.
//!
//! This node does NOT sync blocks - it only participates in transaction gossip.

mod config;
mod forwarder;
mod network;
mod validator;

use clap::Parser;
use config::SentryConfig;
use forwarder::{BackendConfig, TxForwarder};
use network::SentryNetworkConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "reth-sentry-node")]
#[command(about = "Lightweight Ethereum sentry node for mempool tx collection")]
struct Cli {
    /// Path to configuration file (TOML).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// P2P listen port.
    #[arg(long, default_value_t = 30303)]
    port: u16,

    /// Maximum number of peers.
    #[arg(long, default_value_t = 50)]
    max_peers: u32,

    /// Chain ID (1 = mainnet).
    #[arg(long, default_value_t = 1)]
    chain_id: u64,

    /// Backend RPC endpoints to forward transactions to (comma-separated).
    #[arg(long, value_delimiter = ',')]
    backends: Vec<String>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,reth_sentry_node=debug")),
        )
        .init();

    let cli = Cli::parse();

    // Load config from file or CLI args
    let sentry_config = if let Some(config_path) = &cli.config {
        let content = std::fs::read_to_string(config_path)?;
        toml::from_str::<SentryConfig>(&content)?
    } else {
        SentryConfig {
            network: config::NetworkConfigFile {
                chain_id: cli.chain_id,
                max_peers: cli.max_peers,
                p2p_port: cli.port,
                discovery_port: cli.port,
                ..Default::default()
            },
            backend: if cli.backends.is_empty() {
                BackendConfig::default()
            } else {
                BackendConfig {
                    endpoints: cli.backends,
                    ..Default::default()
                }
            },
        }
    };

    info!("=== Reth Sentry Node ===");
    info!("chain_id: {}", sentry_config.network.chain_id);
    info!("p2p_port: {}", sentry_config.network.p2p_port);
    info!("max_peers: {}", sentry_config.network.max_peers);
    info!(
        "backend_endpoints: {:?}",
        sentry_config.backend.endpoints
    );

    // Create the transaction forwarder
    let forwarder = Arc::new(TxForwarder::new(sentry_config.backend));

    // Build network config from file config
    let net_config = SentryNetworkConfig::from(&sentry_config.network);

    // Start the sentry network (blocks until shutdown)
    network::start_sentry_network(net_config, forwarder).await?;

    Ok(())
}
