//! ETH protocol request proxy.
//!
//! Proxies `GetBlockHeaders` and `GetBlockBodies` requests from P2P peers
//! to a backend Ethereum node via WebSocket JSON-RPC.
//!
//! When a WebSocket backend is connected, block data is fetched from it
//! and returned to requesting peers, maintaining good peer reputation.
//! When no backend is available, empty responses are returned as a fallback.

use alloy_consensus::Header;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::B256;
use futures_util::{SinkExt, StreamExt};
use reth_eth_wire::{BlockBodies, BlockHeaders, EthNetworkPrimitives, GetBlockHeaders};
use reth_ethereum_primitives::BlockBody;
use reth_network::eth_requests::IncomingEthRequest;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::{
    connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, warn};

/// JSON-RPC request.
#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: &'static str,
    params: Vec<serde_json::Value>,
    id: u64,
}

/// JSON-RPC response.
#[derive(Deserialize)]
struct JsonRpcResponse {
    id: u64,
    result: Option<serde_json::Value>,
    #[allow(dead_code)]
    error: Option<serde_json::Value>,
}

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Manages a WebSocket connection to a backend node for proxying ETH requests.
struct BackendWsConnection {
    ws_sender: futures_util::stream::SplitSink<WsStream, Message>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Option<serde_json::Value>>>>>,
    id_counter: u64,
}

impl BackendWsConnection {
    /// Send a JSON-RPC request and wait for the response.
    async fn request(
        &mut self,
        method: &'static str,
        params: Vec<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        self.id_counter += 1;
        let id = self.id_counter;

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id,
        };

        let msg = match serde_json::to_string(&req) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to serialize JSON-RPC request");
                return None;
            }
        };

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if self.ws_sender.send(Message::Text(msg.into())).await.is_err() {
            self.pending.lock().await.remove(&id);
            warn!("failed to send request to backend WebSocket");
            return None;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                warn!("backend response channel dropped");
                None
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                warn!("backend request timed out");
                None
            }
        }
    }
}

/// Connect to the backend node via WebSocket and return the connection.
async fn connect_backend(url: &str) -> Option<BackendWsConnection> {
    let (ws_stream, _) = match connect_async(url).await {
        Ok(r) => r,
        Err(e) => {
            error!(url, error = %e, "failed to connect to backend WebSocket");
            return None;
        }
    };

    info!(url, "connected to backend node via WebSocket");

    let (ws_sender, mut ws_receiver) = ws_stream.split();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Option<serde_json::Value>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Spawn a task to read responses and dispatch them
    let pending_clone = pending.clone();
    tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&text) {
                        if let Some(tx) = pending_clone.lock().await.remove(&resp.id) {
                            let _ = tx.send(resp.result);
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    info!("backend WebSocket connection closed");
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "backend WebSocket error");
                    break;
                }
                _ => {}
            }
        }
    });

    Some(BackendWsConnection {
        ws_sender,
        pending,
        id_counter: 0,
    })
}

/// Start the ETH request proxy handler.
///
/// Receives `IncomingEthRequest` from the network and:
/// - If `backend_ws_url` is Some, proxies requests to the backend node
/// - Otherwise, returns empty responses
pub async fn start_eth_proxy(
    mut incoming: mpsc::Receiver<IncomingEthRequest<EthNetworkPrimitives>>,
    backend_ws_url: Option<String>,
) {
    let mut backend = match &backend_ws_url {
        Some(url) => connect_backend(url).await,
        None => {
            info!("no backend WebSocket URL configured, using empty responses for ETH requests");
            None
        }
    };

    while let Some(req) = incoming.recv().await {
        match req {
            IncomingEthRequest::GetBlockHeaders {
                peer_id,
                request,
                response,
            } => {
                let headers = if let Some(ref mut conn) = backend {
                    fetch_headers(conn, &request).await
                } else {
                    vec![]
                };

                debug!(
                    %peer_id,
                    count = headers.len(),
                    "responding to GetBlockHeaders"
                );
                let _ = response.send(Ok(BlockHeaders(headers)));
            }
            IncomingEthRequest::GetBlockBodies {
                peer_id,
                request,
                response,
            } => {
                let bodies = if let Some(ref mut conn) = backend {
                    fetch_bodies(conn, &request.0).await
                } else {
                    vec![]
                };

                debug!(
                    %peer_id,
                    count = bodies.len(),
                    "responding to GetBlockBodies"
                );
                let _ = response.send(Ok(BlockBodies(bodies)));
            }
            IncomingEthRequest::GetNodeData { .. } => {
                // NodeData is deprecated in eth/67+, ignore
            }
            IncomingEthRequest::GetReceipts {
                response, ..
            } => {
                let _ = response.send(Ok(Default::default()));
            }
            IncomingEthRequest::GetReceipts69 {
                response, ..
            } => {
                let _ = response.send(Ok(reth_eth_wire::Receipts69(vec![])));
            }
            IncomingEthRequest::GetReceipts70 {
                response, ..
            } => {
                let _ = response.send(Ok(reth_eth_wire::Receipts70 {
                    last_block_incomplete: false,
                    receipts: vec![],
                }));
            }
        }
    }
}

/// Fetch block headers from backend via JSON-RPC.
async fn fetch_headers(
    conn: &mut BackendWsConnection,
    request: &GetBlockHeaders,
) -> Vec<Header> {
    let mut headers = Vec::new();
    let GetBlockHeaders {
        start_block,
        limit,
        skip,
        direction,
    } = request;

    // For simplicity, fetch headers one by one.
    // Most peer requests are for a small number of headers.
    let mut block_num_or_hash: Option<serde_json::Value> = match start_block {
        BlockHashOrNumber::Hash(h) => Some(serde_json::Value::String(format!("0x{:x}", h))),
        BlockHashOrNumber::Number(n) => Some(serde_json::Value::String(format!("0x{:x}", n))),
    };

    for _ in 0..*limit {
        let block_id = match block_num_or_hash.take() {
            Some(id) => id,
            None => break,
        };

        let result = conn
            .request("eth_getBlockByNumber", vec![block_id.clone(), false.into()])
            .await;

        let result = match result {
            Some(v) if !v.is_null() => v,
            _ => break,
        };

        // Parse header from JSON-RPC response
        match serde_json::from_value::<Header>(result) {
            Ok(header) => {
                let next_num = header.number;
                headers.push(header);

                // Calculate next block number
                let step = (*skip as u64) + 1;
                let next = if direction.is_rising() {
                    next_num.checked_add(step)
                } else {
                    next_num.checked_sub(step)
                };

                block_num_or_hash = next
                    .map(|n| serde_json::Value::String(format!("0x{:x}", n)));
            }
            Err(e) => {
                debug!(error = %e, "failed to parse header from backend");
                break;
            }
        }
    }

    headers
}

/// Fetch block bodies from backend via JSON-RPC.
async fn fetch_bodies(
    conn: &mut BackendWsConnection,
    hashes: &[B256],
) -> Vec<BlockBody> {
    let mut bodies = Vec::new();

    for hash in hashes {
        let hash_str = serde_json::Value::String(format!("0x{:x}", hash));
        let result = conn
            .request("eth_getBlockByHash", vec![hash_str, true.into()])
            .await;

        let block_json = match result {
            Some(v) if !v.is_null() => v,
            _ => continue,
        };

        // Extract transactions and uncles from the full block
        match parse_block_body(&block_json) {
            Some(body) => bodies.push(body),
            None => {
                debug!(hash = %hash, "failed to parse block body from backend");
            }
        }
    }

    bodies
}

/// Parse a BlockBody from a JSON-RPC block response.
fn parse_block_body(block_json: &serde_json::Value) -> Option<BlockBody> {
    // Try to deserialize directly
    serde_json::from_value::<BlockBody>(block_json.clone()).ok()
}
