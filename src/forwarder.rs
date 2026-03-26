//! Transaction forwarder - sends validated transactions to backend nodes.

use crate::dedup::{DedupConfig, TxDedup};
use crate::ws_server::{PendingTxNotification, WsBroadcaster};
use alloy_primitives::B256;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// A validated transaction ready to be forwarded.
#[derive(Debug, Clone)]
pub struct ForwardableTx {
    /// Transaction hash.
    pub hash: B256,
    /// RLP-encoded signed transaction (raw tx bytes).
    pub raw_tx: Vec<u8>,
}

/// Configuration for backend endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    /// List of backend node RPC URLs to forward transactions to.
    pub endpoints: Vec<String>,
    /// Maximum number of concurrent forwarding tasks.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Channel buffer size for pending forwards.
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
    /// Deduplication configuration.
    #[serde(default)]
    pub dedup: DedupConfig,
}

fn default_max_concurrent() -> usize {
    64
}

fn default_buffer_size() -> usize {
    4096
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            endpoints: vec!["http://localhost:8545".to_string()],
            max_concurrent: default_max_concurrent(),
            buffer_size: default_buffer_size(),
            dedup: DedupConfig::default(),
        }
    }
}

/// JSON-RPC request for eth_sendRawTransaction.
#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: &'static str,
    params: Vec<String>,
    id: u64,
}

/// JSON-RPC response.
#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// Transaction forwarder that sends raw transactions to backend nodes via:
/// - HTTP JSON-RPC (`eth_sendRawTransaction`) to configured endpoints
/// - WebSocket broadcast to connected subscribers
pub struct TxForwarder {
    tx_sender: mpsc::Sender<ForwardableTx>,
}

impl TxForwarder {
    /// Create a new forwarder and spawn the background forwarding loop.
    ///
    /// If `ws_broadcaster` is provided, transactions will also be broadcast
    /// to all connected WebSocket subscribers.
    pub fn new(config: BackendConfig, ws_broadcaster: Option<WsBroadcaster>) -> Self {
        let (tx_sender, rx) = mpsc::channel(config.buffer_size);
        let forwarder = Self { tx_sender };

        // Spawn the forwarding task
        tokio::spawn(Self::forwarding_loop(config, rx, ws_broadcaster));

        forwarder
    }

    /// Send a transaction to be forwarded to backend nodes.
    pub async fn forward(&self, tx: ForwardableTx) {
        if let Err(e) = self.tx_sender.send(tx).await {
            warn!("failed to queue transaction for forwarding: {}", e);
        }
    }

    /// Background loop that consumes transactions and forwards them.
    async fn forwarding_loop(
        config: BackendConfig,
        mut rx: mpsc::Receiver<ForwardableTx>,
        ws_broadcaster: Option<WsBroadcaster>,
    ) {
        let client = Client::new();
        let endpoints: Arc<Vec<String>> = Arc::new(config.endpoints);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent));
        let dedup = TxDedup::new(config.dedup);
        let mut id_counter: u64 = 0;

        info!(
            endpoints = ?endpoints.as_ref(),
            "transaction forwarder started"
        );

        while let Some(tx) = rx.recv().await {
            // Deduplicate: skip if we've already seen this tx hash
            if !dedup.check_and_insert(tx.hash) {
                debug!(tx_hash = %tx.hash, "duplicate transaction skipped");
                continue;
            }

            // Broadcast to WebSocket subscribers
            if let Some(ref ws) = ws_broadcaster {
                ws.broadcast(PendingTxNotification {
                    hash: tx.hash,
                    raw_tx: tx.raw_tx.clone(),
                });
            }

            // Forward to HTTP RPC endpoints
            if endpoints.is_empty() {
                continue;
            }

            let client = client.clone();
            let endpoints = endpoints.clone();
            let permit = semaphore.clone().acquire_owned().await;

            if permit.is_err() {
                warn!("semaphore closed, stopping forwarder");
                break;
            }
            let permit = permit.unwrap();

            id_counter += 1;
            let id = id_counter;

            tokio::spawn(async move {
                let raw_hex = format!("0x{}", alloy_primitives::hex::encode(&tx.raw_tx));

                for endpoint in endpoints.iter() {
                    let request = JsonRpcRequest {
                        jsonrpc: "2.0",
                        method: "eth_sendRawTransaction",
                        params: vec![raw_hex.clone()],
                        id,
                    };

                    match client.post(endpoint).json(&request).send().await {
                        Ok(response) => {
                            match response.json::<JsonRpcResponse>().await {
                                Ok(rpc_resp) => {
                                    if let Some(err) = rpc_resp.error {
                                        // "already known" is expected and not a real error
                                        if err.message.contains("already known")
                                            || err.message.contains("already in pool")
                                        {
                                            debug!(
                                                tx_hash = %tx.hash,
                                                endpoint,
                                                "transaction already known by backend"
                                            );
                                        } else {
                                            warn!(
                                                tx_hash = %tx.hash,
                                                endpoint,
                                                code = err.code,
                                                message = %err.message,
                                                "backend rejected transaction"
                                            );
                                        }
                                    } else {
                                        debug!(
                                            tx_hash = %tx.hash,
                                            endpoint,
                                            "transaction forwarded successfully"
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        tx_hash = %tx.hash,
                                        endpoint,
                                        error = %e,
                                        "failed to parse backend response"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!(
                                tx_hash = %tx.hash,
                                endpoint,
                                error = %e,
                                "failed to send transaction to backend"
                            );
                        }
                    }
                }

                drop(permit);
            });
        }

        info!("transaction forwarder stopped");
    }
}
