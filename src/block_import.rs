//! Custom BlockImport that caches NewBlock announcements.
//!
//! Instead of importing blocks into a database, we just store them
//! in an LRU cache so we can serve them to peers.

use crate::block_cache::BlockCache;
use reth_eth_wire::NewBlock;
use reth_network::import::{BlockImport, BlockImportEvent, NewBlockEvent};
use reth_network_peers::PeerId;
use std::task::{Context, Poll};

/// A BlockImport implementation that caches received blocks.
#[derive(Debug)]
pub struct CachingBlockImport {
    cache: BlockCache,
}

impl CachingBlockImport {
    pub fn new(cache: BlockCache) -> Self {
        Self { cache }
    }
}

impl BlockImport<NewBlock> for CachingBlockImport {
    fn on_new_block(&mut self, _peer_id: PeerId, incoming_block: NewBlockEvent<NewBlock>) {
        if let NewBlockEvent::Block(msg) = incoming_block {
            let block = &msg.block.block;
            let hash = msg.hash;
            let header = block.header.clone();
            let body = block.body.clone();
            self.cache.insert(hash, header, body);
        }
        // NewBlockHashes: we don't fetch blocks from peers, so just ignore
    }

    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<BlockImportEvent<NewBlock>> {
        // We never produce import events since we don't validate blocks
        Poll::Pending
    }
}
