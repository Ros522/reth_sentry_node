//! ETH protocol request handler using block cache.
//!
//! Responds to `GetBlockHeaders` and `GetBlockBodies` requests using
//! blocks cached from devp2p NewBlock announcements.
//! Returns empty responses on cache miss.

use crate::block_cache::BlockCache;
use alloy_eips::BlockHashOrNumber;
use reth_eth_wire::{BlockBodies, BlockHeaders, EthNetworkPrimitives, GetBlockHeaders};
use reth_network::eth_requests::IncomingEthRequest;
use tokio::sync::mpsc;
use tracing::debug;

/// Start the ETH request handler.
///
/// Receives `IncomingEthRequest` from the network and responds using
/// the block cache. Returns empty responses on cache miss.
pub async fn start_eth_request_handler(
    mut incoming: mpsc::Receiver<IncomingEthRequest<EthNetworkPrimitives>>,
    cache: BlockCache,
) {
    while let Some(req) = incoming.recv().await {
        match req {
            IncomingEthRequest::GetBlockHeaders {
                peer_id,
                request,
                response,
            } => {
                let headers = resolve_headers(&cache, &request);
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
                let bodies = resolve_bodies(&cache, &request.0);
                debug!(
                    %peer_id,
                    count = bodies.len(),
                    "responding to GetBlockBodies"
                );
                let _ = response.send(Ok(BlockBodies(bodies)));
            }
            IncomingEthRequest::GetNodeData { .. } => {}
            IncomingEthRequest::GetReceipts { response, .. } => {
                let _ = response.send(Ok(Default::default()));
            }
            IncomingEthRequest::GetReceipts69 { response, .. } => {
                let _ = response.send(Ok(reth_eth_wire::Receipts69(vec![])));
            }
            IncomingEthRequest::GetReceipts70 { response, .. } => {
                let _ = response.send(Ok(reth_eth_wire::Receipts70 {
                    last_block_incomplete: false,
                    receipts: vec![],
                }));
            }
        }
    }
}

/// Resolve block headers from the cache.
fn resolve_headers(
    cache: &BlockCache,
    request: &GetBlockHeaders,
) -> Vec<alloy_consensus::Header> {
    let mut headers = Vec::new();
    let GetBlockHeaders {
        start_block,
        limit,
        skip,
        direction,
    } = request;

    // Resolve the starting block number
    let mut current_number: Option<u64> = match start_block {
        BlockHashOrNumber::Number(n) => Some(*n),
        BlockHashOrNumber::Hash(h) => {
            // Try to find by hash, get its number
            cache.get_header_by_hash(h).map(|hdr| hdr.number)
        }
    };

    let step = (*skip as u64) + 1;

    for _ in 0..*limit {
        let num = match current_number {
            Some(n) => n,
            None => break,
        };

        if let Some(header) = cache.get_header_by_number(num) {
            headers.push(header);
        } else {
            // Cache miss — stop here
            break;
        }

        current_number = if direction.is_rising() {
            num.checked_add(step)
        } else {
            num.checked_sub(step)
        };
    }

    headers
}

/// Resolve block bodies from the cache.
fn resolve_bodies(
    cache: &BlockCache,
    hashes: &[alloy_primitives::B256],
) -> Vec<reth_ethereum_primitives::BlockBody> {
    let mut bodies = Vec::new();
    for hash in hashes {
        if let Some(body) = cache.get_body_by_hash(hash) {
            bodies.push(body);
        }
        // On cache miss, skip (peer will get a shorter response, which is valid)
    }
    bodies
}
