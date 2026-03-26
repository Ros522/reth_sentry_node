//! Block cache for storing blocks received via devp2p NewBlock messages.
//!
//! Caches recent block headers and bodies so the sentry node can respond
//! to peer GetBlockHeaders/GetBlockBodies requests without a backend node.

use alloy_consensus::Header;
use alloy_primitives::B256;
use lru::LruCache;
use reth_ethereum_primitives::BlockBody;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use tracing::debug;

/// Default number of blocks to cache.
const DEFAULT_CACHE_SIZE: usize = 256;

/// Shared block cache accessible from multiple tasks.
#[derive(Clone)]
pub struct BlockCache {
    inner: Arc<Mutex<BlockCacheInner>>,
}

impl std::fmt::Debug for BlockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockCache").finish()
    }
}

struct BlockCacheInner {
    /// Header by block hash.
    headers_by_hash: LruCache<B256, Header>,
    /// Header by block number.
    headers_by_number: LruCache<u64, Header>,
    /// Block hash by block number (for lookups).
    hash_by_number: LruCache<u64, B256>,
    /// Body by block hash.
    bodies_by_hash: LruCache<B256, BlockBody>,
}

impl BlockCache {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(DEFAULT_CACHE_SIZE).unwrap());
        Self {
            inner: Arc::new(Mutex::new(BlockCacheInner {
                headers_by_hash: LruCache::new(cap),
                headers_by_number: LruCache::new(cap),
                hash_by_number: LruCache::new(cap),
                bodies_by_hash: LruCache::new(cap),
            })),
        }
    }

    /// Insert a block (header + body) into the cache.
    pub fn insert(&self, hash: B256, header: Header, body: BlockBody) {
        let mut inner = self.inner.lock().unwrap();
        let number = header.number;
        inner.headers_by_hash.push(hash, header.clone());
        inner.headers_by_number.push(number, header);
        inner.hash_by_number.push(number, hash);
        inner.bodies_by_hash.push(hash, body);
        debug!(block_number = number, %hash, "cached block from NewBlock");
    }

    /// Get a header by block hash.
    pub fn get_header_by_hash(&self, hash: &B256) -> Option<Header> {
        self.inner.lock().unwrap().headers_by_hash.get(hash).cloned()
    }

    /// Get a header by block number.
    pub fn get_header_by_number(&self, number: u64) -> Option<Header> {
        self.inner.lock().unwrap().headers_by_number.get(&number).cloned()
    }

    /// Get a body by block hash.
    pub fn get_body_by_hash(&self, hash: &B256) -> Option<BlockBody> {
        self.inner.lock().unwrap().bodies_by_hash.get(hash).cloned()
    }

    /// Get the block hash for a given block number.
    pub fn get_hash_by_number(&self, number: u64) -> Option<B256> {
        self.inner.lock().unwrap().hash_by_number.get(&number).copied()
    }
}
