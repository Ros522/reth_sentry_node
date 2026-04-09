//! ETH protocol request handler using block cache with peer proxy fallback.
//!
//! Responds to `GetBlockHeaders` and `GetBlockBodies` requests using
//! blocks cached from devp2p NewBlock announcements.
//! On cache miss, proxies the request to internet peers via FetchClient.

use crate::block_cache::BlockCache;
use alloy_eips::BlockHashOrNumber;
use reth_eth_wire::{BlockBodies, BlockHeaders, EthNetworkPrimitives, GetBlockHeaders};
use reth_network::eth_requests::IncomingEthRequest;
use reth_network::FetchClient;
use reth_network_p2p::bodies::client::BodiesClient;
use reth_network_p2p::headers::client::{HeadersClient, HeadersRequest};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Start the ETH request handler.
///
/// Responds from cache, falls back to proxying to internet peers on cache miss.
pub async fn start_eth_request_handler(
    mut incoming: mpsc::Receiver<IncomingEthRequest<EthNetworkPrimitives>>,
    cache: BlockCache,
    fetch_client: FetchClient<EthNetworkPrimitives>,
) {
    while let Some(req) = incoming.recv().await {
        match req {
            IncomingEthRequest::GetBlockHeaders {
                peer_id,
                request,
                response,
            } => {
                let requested = format!("{:?} limit={} skip={}", request.start_block, request.limit, request.skip);

                // Try cache first
                let headers = resolve_headers_from_cache(&cache, &request);
                if !headers.is_empty() {
                    info!(
                        %peer_id,
                        request = %requested,
                        count = headers.len(),
                        "GetBlockHeaders: from cache"
                    );
                    let _ = response.send(Ok(BlockHeaders(headers)));
                    continue;
                }

                // Cache miss → proxy to internet peers
                debug!(
                    %peer_id,
                    request = %requested,
                    "GetBlockHeaders: cache miss, proxying to peers"
                );

                let proxy_request = HeadersRequest {
                    start: request.start_block,
                    limit: request.limit,
                    direction: request.direction,
                };

                match fetch_client.get_headers(proxy_request).await {
                    Ok(peer_response) => {
                        let fetched_headers = peer_response.into_data();
                        info!(
                            %peer_id,
                            request = %requested,
                            count = fetched_headers.len(),
                            "GetBlockHeaders: proxied from internet peer"
                        );

                        // Cache the fetched headers
                        for header in &fetched_headers {
                            let hash = header.hash_slow();
                            cache.insert_header(hash, header.clone());
                        }

                        let _ = response.send(Ok(BlockHeaders(fetched_headers)));
                    }
                    Err(e) => {
                        warn!(
                            %peer_id,
                            request = %requested,
                            error = %e,
                            "GetBlockHeaders: proxy failed, returning empty"
                        );
                        let _ = response.send(Ok(BlockHeaders(vec![])));
                    }
                }
            }
            IncomingEthRequest::GetBlockBodies {
                peer_id,
                request,
                response,
            } => {
                let requested_count = request.0.len();

                // Try cache first
                let bodies = resolve_bodies_from_cache(&cache, &request.0);
                if bodies.len() == requested_count && requested_count > 0 {
                    debug!(
                        %peer_id,
                        count = bodies.len(),
                        "GetBlockBodies: from cache"
                    );
                    let _ = response.send(Ok(BlockBodies(bodies)));
                    continue;
                }

                // Cache miss (partial or full) → proxy to internet peers
                let missing_hashes: Vec<_> = request.0.iter()
                    .filter(|h| cache.get_body_by_hash(h).is_none())
                    .cloned()
                    .collect();

                if missing_hashes.is_empty() {
                    let _ = response.send(Ok(BlockBodies(bodies)));
                    continue;
                }

                debug!(
                    %peer_id,
                    missing = missing_hashes.len(),
                    total = requested_count,
                    "GetBlockBodies: cache miss, proxying to peers"
                );

                match fetch_client.get_block_bodies(missing_hashes).await {
                    Ok(peer_response) => {
                        let fetched_bodies = peer_response.into_data();
                        info!(
                            %peer_id,
                            fetched = fetched_bodies.len(),
                            "GetBlockBodies: proxied from internet peer"
                        );

                        // Re-resolve all requested bodies (cache + newly fetched)
                        // The fetched bodies are in order of the missing hashes,
                        // but we need to return them in order of the original request.
                        // For simplicity, cache them and re-resolve.
                        // Note: we can't easily cache bodies without knowing their hash,
                        // so we return the proxy result directly for missing ones.

                        // Build the full response by combining cache and proxied
                        let mut all_bodies = Vec::new();
                        let mut fetch_iter = fetched_bodies.into_iter();
                        for hash in &request.0 {
                            if let Some(body) = cache.get_body_by_hash(hash) {
                                all_bodies.push(body);
                            } else if let Some(body) = fetch_iter.next() {
                                // Cache it for future use
                                cache.insert_body(*hash, body.clone());
                                all_bodies.push(body);
                            }
                        }

                        let _ = response.send(Ok(BlockBodies(all_bodies)));
                    }
                    Err(e) => {
                        warn!(
                            %peer_id,
                            error = %e,
                            "GetBlockBodies: proxy failed, returning partial from cache"
                        );
                        let _ = response.send(Ok(BlockBodies(bodies)));
                    }
                }
            }
            IncomingEthRequest::GetNodeData { .. } => {
                debug!("GetNodeData request (ignored, deprecated)");
            }
            IncomingEthRequest::GetReceipts {
                peer_id, response, ..
            } => {
                debug!(%peer_id, "GetReceipts: returning empty");
                let _ = response.send(Ok(Default::default()));
            }
            IncomingEthRequest::GetReceipts69 {
                peer_id, response, ..
            } => {
                debug!(%peer_id, "GetReceipts69: returning empty");
                let _ = response.send(Ok(reth_eth_wire::Receipts69(vec![])));
            }
            IncomingEthRequest::GetReceipts70 {
                peer_id, response, ..
            } => {
                debug!(%peer_id, "GetReceipts70: returning empty");
                let _ = response.send(Ok(reth_eth_wire::Receipts70 {
                    last_block_incomplete: false,
                    receipts: vec![],
                }));
            }
            IncomingEthRequest::GetBlockAccessLists {
                response, ..
            } => {
                let _ = response.send(Ok(Default::default()));
            }
        }
    }
}

/// Resolve block headers from the cache only.
fn resolve_headers_from_cache(
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

    let mut current_number: Option<u64> = match start_block {
        BlockHashOrNumber::Number(n) => Some(*n),
        BlockHashOrNumber::Hash(h) => cache.get_header_by_hash(h).map(|hdr| hdr.number),
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

/// Resolve block bodies from the cache only.
fn resolve_bodies_from_cache(
    cache: &BlockCache,
    hashes: &[alloy_primitives::B256],
) -> Vec<reth_ethereum_primitives::BlockBody> {
    let mut bodies = Vec::new();
    for hash in hashes {
        if let Some(body) = cache.get_body_by_hash(hash) {
            bodies.push(body);
        }
    }
    bodies
}
