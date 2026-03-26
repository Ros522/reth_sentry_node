//! Configuration for the sentry node.

use crate::forwarder::BackendConfig;
use crate::network::SentryNetworkConfig;
use crate::ws_server::WsConfig;
use serde::{Deserialize, Serialize};

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentryConfig {
    /// Network configuration.
    #[serde(default)]
    pub network: NetworkConfigFile,
    /// Backend forwarding configuration.
    #[serde(default)]
    pub backend: BackendConfig,
    /// WebSocket server configuration.
    #[serde(default)]
    pub websocket: WsConfig,
    /// Backend node WebSocket URL for proxying ETH protocol requests.
    /// When set, GetBlockHeaders/GetBlockBodies are proxied to this node.
    #[serde(default)]
    pub backend_ws: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfigFile {
    /// Chain ID (1 = mainnet).
    #[serde(default = "default_chain_id")]
    pub chain_id: u64,
    /// Maximum number of peers.
    #[serde(default = "default_max_peers")]
    pub max_peers: u32,
    /// P2P listen port.
    #[serde(default = "default_port")]
    pub p2p_port: u16,
    /// Discovery (UDP) port.
    #[serde(default = "default_port")]
    pub discovery_port: u16,
}

fn default_chain_id() -> u64 {
    1
}
fn default_max_peers() -> u32 {
    50
}
fn default_port() -> u16 {
    30303
}

impl Default for NetworkConfigFile {
    fn default() -> Self {
        Self {
            chain_id: default_chain_id(),
            max_peers: default_max_peers(),
            p2p_port: default_port(),
            discovery_port: default_port(),
        }
    }
}

impl Default for SentryConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfigFile::default(),
            backend: BackendConfig::default(),
            websocket: WsConfig::default(),
            backend_ws: None,
        }
    }
}

impl From<&NetworkConfigFile> for SentryNetworkConfig {
    fn from(cfg: &NetworkConfigFile) -> Self {
        Self {
            chain_id: cfg.chain_id,
            max_peers: cfg.max_peers,
            p2p_port: cfg.p2p_port,
            discovery_port: cfg.discovery_port,
        }
    }
}
