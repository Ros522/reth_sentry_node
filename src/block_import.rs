//! Custom BlockImport that caches NewBlock announcements and rebroadcasts them.
//!
//! Instead of importing blocks into a database, we store them
//! in an LRU cache so we can serve them to peers, and rebroadcast
//! to all connected peers so backend nodes can follow the chain tip.

use crate::block_cache::BlockCache;
use reth_eth_wire::NewBlock;
use reth_network::import::{BlockImport, BlockImportEvent, NewBlockEvent};
use reth_network_peers::PeerId;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// Data sent from BlockImport to the rebroadcast task.
#[derive(Clone)]
pub struct NewBlockData {
    pub hash: alloy_primitives::B256,
    pub block: std::sync::Arc<NewBlock>,
}

/// A BlockImport implementation that caches received blocks and
/// sends them to a rebroadcast channel.
#[derive(Debug)]
pub struct CachingBlockImport {
    cache: BlockCache,
    rebroadcast_tx: mpsc::UnboundedSender<NewBlockData>,
}

impl CachingBlockImport {
    pub fn new(cache: BlockCache, rebroadcast_tx: mpsc::UnboundedSender<NewBlockData>) -> Self {
        Self {
            cache,
            rebroadcast_tx,
        }
    }
}

impl BlockImport<NewBlock> for CachingBlockImport {
    fn on_new_block(&mut self, _peer_id: PeerId, incoming_block: NewBlockEvent<NewBlock>) {
        if let NewBlockEvent::Block(msg) = incoming_block {
            let block = &msg.block.block;
            let hash = msg.hash;
            let header = block.header.clone();
            let body = block.body.clone();

            // Cache the block
            self.cache.insert(hash, header, body);

            // Send to rebroadcast task
            let _ = self.rebroadcast_tx.send(NewBlockData {
                hash,
                block: msg.block.clone(),
            });
        }
        // NewBlockHashes: we don't fetch blocks from peers, so just ignore
    }

    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<BlockImportEvent<NewBlock>> {
        Poll::Pending
    }
}
