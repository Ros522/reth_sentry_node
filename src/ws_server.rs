//! WebSocket server for streaming pending transactions to backend subscribers.
//!
//! Backends connect via WebSocket and receive pending transactions as they arrive.
//! Two subscription modes are supported:
//! - `hash`: receive only transaction hashes (lightweight)
//! - `raw`: receive full raw signed transaction bytes (hex-encoded)
//!
//! ## Protocol
//!
//! After connecting, the client sends a JSON subscription message:
//! ```json
//! {"subscribe": "pending_transactions", "mode": "raw"}
//! ```
//! or
//! ```json
//! {"subscribe": "pending_transactions", "mode": "hash"}
//! ```
//!
//! The server then streams JSON messages:
//! - hash mode: `{"hash": "0x..."}`
//! - raw mode:  `{"hash": "0x...", "raw_tx": "0x..."}`

use alloy_primitives::B256;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// A pending transaction notification broadcast to WebSocket subscribers.
#[derive(Debug, Clone)]
pub struct PendingTxNotification {
    /// Transaction hash.
    pub hash: B256,
    /// RLP-encoded raw signed transaction bytes.
    pub raw_tx: Vec<u8>,
}

/// Subscription request from a WebSocket client.
#[derive(Deserialize)]
struct SubscribeRequest {
    subscribe: String,
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "raw".to_string()
}

/// Hash-only notification sent to clients.
#[derive(Serialize)]
struct HashNotification {
    hash: String,
}

/// Full raw transaction notification sent to clients.
#[derive(Serialize)]
struct RawTxNotification {
    hash: String,
    raw_tx: String,
}

/// Configuration for the WebSocket server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsConfig {
    /// Whether to enable the WebSocket server.
    #[serde(default = "default_ws_enabled")]
    pub enabled: bool,
    /// Address to bind the WebSocket server to.
    #[serde(default = "default_ws_addr")]
    pub addr: String,
    /// Port for the WebSocket server.
    #[serde(default = "default_ws_port")]
    pub port: u16,
    /// Broadcast channel capacity.
    #[serde(default = "default_ws_capacity")]
    pub capacity: usize,
}

fn default_ws_enabled() -> bool {
    true
}

fn default_ws_addr() -> String {
    "0.0.0.0".to_string()
}

fn default_ws_port() -> u16 {
    8546
}

fn default_ws_capacity() -> usize {
    4096
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            enabled: default_ws_enabled(),
            addr: default_ws_addr(),
            port: default_ws_port(),
            capacity: default_ws_capacity(),
        }
    }
}

/// WebSocket server handle for broadcasting pending transactions.
#[derive(Clone)]
pub struct WsBroadcaster {
    sender: broadcast::Sender<PendingTxNotification>,
}

impl WsBroadcaster {
    /// Broadcast a pending transaction to all connected WebSocket clients.
    pub fn broadcast(&self, notification: PendingTxNotification) {
        // broadcast::send returns Err only if there are no receivers, which is fine
        let _ = self.sender.send(notification);
    }
}

/// Start the WebSocket server and return a broadcaster handle.
pub async fn start_ws_server(config: WsConfig) -> eyre::Result<WsBroadcaster> {
    let (sender, _) = broadcast::channel(config.capacity);
    let broadcaster = WsBroadcaster {
        sender: sender.clone(),
    };

    let bind_addr: SocketAddr = format!("{}:{}", config.addr, config.port).parse()?;
    let listener = TcpListener::bind(bind_addr).await?;

    info!(%bind_addr, "WebSocket server started, waiting for backend connections");

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    info!(%peer_addr, "new WebSocket client connected");
                    let rx = sender.subscribe();
                    tokio::spawn(handle_ws_connection(stream, peer_addr, rx));
                }
                Err(e) => {
                    error!(error = %e, "failed to accept WebSocket connection");
                }
            }
        }
    });

    Ok(broadcaster)
}

/// Handle a single WebSocket client connection.
async fn handle_ws_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    mut rx: broadcast::Receiver<PendingTxNotification>,
) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!(%peer_addr, error = %e, "WebSocket handshake failed");
            return;
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Wait for the subscription message from the client
    let mode = match wait_for_subscription(&mut ws_receiver, peer_addr).await {
        Some(m) => m,
        None => return,
    };

    let use_raw = mode == "raw" || mode == "full";

    info!(
        %peer_addr,
        %mode,
        "client subscribed to pending_transactions"
    );

    // Send a confirmation
    let confirm = serde_json::json!({"status": "subscribed", "mode": mode});
    if ws_sender
        .send(Message::Text(confirm.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    // Stream pending transactions to the client
    loop {
        match rx.recv().await {
            Ok(notification) => {
                let msg = if use_raw {
                    serde_json::to_string(&RawTxNotification {
                        hash: format!("0x{}", alloy_primitives::hex::encode(notification.hash)),
                        raw_tx: format!(
                            "0x{}",
                            alloy_primitives::hex::encode(&notification.raw_tx)
                        ),
                    })
                } else {
                    serde_json::to_string(&HashNotification {
                        hash: format!("0x{}", alloy_primitives::hex::encode(notification.hash)),
                    })
                };

                match msg {
                    Ok(json) => {
                        if ws_sender
                            .send(Message::Text(json.into()))
                            .await
                            .is_err()
                        {
                            debug!(%peer_addr, "WebSocket client disconnected (send failed)");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(%peer_addr, error = %e, "failed to serialize notification");
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(%peer_addr, skipped = n, "client lagging, skipped transactions");
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!(%peer_addr, "broadcast channel closed, disconnecting client");
                break;
            }
        }
    }
}

/// Wait for the client to send a subscription request, with a timeout.
async fn wait_for_subscription<S>(
    ws_receiver: &mut S,
    peer_addr: SocketAddr,
) -> Option<String>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let timeout = tokio::time::Duration::from_secs(10);

    match tokio::time::timeout(timeout, ws_receiver.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            match serde_json::from_str::<SubscribeRequest>(&text) {
                Ok(req) if req.subscribe == "pending_transactions" => Some(req.mode),
                Ok(req) => {
                    warn!(
                        %peer_addr,
                        subscribe = %req.subscribe,
                        "unknown subscription type, defaulting to pending_transactions"
                    );
                    Some(req.mode)
                }
                Err(_) => {
                    // If client doesn't send a proper subscribe message,
                    // default to raw mode (permissive for simple clients)
                    debug!(%peer_addr, "no valid subscribe message, defaulting to raw mode");
                    Some("raw".to_string())
                }
            }
        }
        Ok(Some(Ok(_))) => {
            // Non-text message, default to raw
            debug!(%peer_addr, "non-text first message, defaulting to raw mode");
            Some("raw".to_string())
        }
        Ok(Some(Err(e))) => {
            warn!(%peer_addr, error = %e, "WebSocket error during subscription");
            None
        }
        Ok(None) => {
            debug!(%peer_addr, "WebSocket closed before subscription");
            None
        }
        Err(_) => {
            // Timeout - default to raw mode (permissive)
            debug!(%peer_addr, "subscription timeout, defaulting to raw mode");
            Some("raw".to_string())
        }
    }
}
