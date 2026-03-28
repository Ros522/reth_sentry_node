//! Block cache for storing blocks received via devp2p NewBlock messages.
//!
//! Caches recent block headers and bodies so the sentry node can respond
//! to peer GetBlockHeaders/GetBlockBodies requests without a backend node.
//!
//! The cache can be persisted to disk on shutdown and restored on startup.

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable};
use lru::LruCache;
use reth_ethereum_primitives::BlockBody;
use std::io::{Read as _, Write as _};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Default number of blocks to cache.
const DEFAULT_CACHE_SIZE: usize = 256;

/// A cached block entry for persistence.
struct CachedBlock {
    hash: B256,
    header: Header,
    body: BlockBody,
}

/// Shared block cache accessible from multiple tasks.
#[derive(Clone)]
pub struct BlockCache {
    inner: Arc<Mutex<BlockCacheInner>>,
    capacity: usize,
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
        let cap =
            NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(DEFAULT_CACHE_SIZE).unwrap());
        Self {
            inner: Arc::new(Mutex::new(BlockCacheInner {
                headers_by_hash: LruCache::new(cap),
                headers_by_number: LruCache::new(cap),
                hash_by_number: LruCache::new(cap),
                bodies_by_hash: LruCache::new(cap),
            })),
            capacity,
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
        info!(block_number = number, %hash, "cached block from NewBlock");
    }

    /// Insert just a header (from proxy response).
    pub fn insert_header(&self, hash: B256, header: Header) {
        let mut inner = self.inner.lock().unwrap();
        let number = header.number;
        inner.headers_by_hash.push(hash, header.clone());
        inner.headers_by_number.push(number, header);
        inner.hash_by_number.push(number, hash);
    }

    /// Insert just a body (from proxy response).
    pub fn insert_body(&self, hash: B256, body: BlockBody) {
        let mut inner = self.inner.lock().unwrap();
        inner.bodies_by_hash.push(hash, body);
    }

    /// Get a header by block hash.
    pub fn get_header_by_hash(&self, hash: &B256) -> Option<Header> {
        self.inner.lock().unwrap().headers_by_hash.get(hash).cloned()
    }

    /// Get a header by block number.
    pub fn get_header_by_number(&self, number: u64) -> Option<Header> {
        self.inner
            .lock()
            .unwrap()
            .headers_by_number
            .get(&number)
            .cloned()
    }

    /// Get a body by block hash.
    pub fn get_body_by_hash(&self, hash: &B256) -> Option<BlockBody> {
        self.inner
            .lock()
            .unwrap()
            .bodies_by_hash
            .get(hash)
            .cloned()
    }

    /// Get the block hash for a given block number.
    pub fn get_hash_by_number(&self, number: u64) -> Option<B256> {
        self.inner
            .lock()
            .unwrap()
            .hash_by_number
            .get(&number)
            .copied()
    }

    /// Get the latest (highest block number) cached block's header and hash.
    pub fn latest(&self) -> Option<(B256, Header)> {
        let inner = self.inner.lock().unwrap();
        inner
            .hash_by_number
            .iter()
            .max_by_key(|(&num, _)| num)
            .and_then(|(&num, &hash)| {
                inner
                    .headers_by_number
                    .peek(&num)
                    .map(|h| (hash, h.clone()))
            })
    }

    /// Save the cache to a file.
    ///
    /// Format: [count: u32] [block_entry]*
    /// Each entry: [hash: 32 bytes] [header_len: u32] [header_rlp] [body_len: u32] [body_rlp]
    pub fn save_to_file(&self, path: &Path) -> eyre::Result<()> {
        let blocks = self.drain_blocks();
        let count = blocks.len();

        let mut file = std::fs::File::create(path)?;

        // Write count
        file.write_all(&(count as u32).to_le_bytes())?;

        for block in &blocks {
            // Hash
            file.write_all(block.hash.as_ref())?;

            // Header RLP
            let mut header_rlp = Vec::new();
            block.header.encode(&mut header_rlp);
            file.write_all(&(header_rlp.len() as u32).to_le_bytes())?;
            file.write_all(&header_rlp)?;

            // Body RLP
            let mut body_rlp = Vec::new();
            block.body.encode(&mut body_rlp);
            file.write_all(&(body_rlp.len() as u32).to_le_bytes())?;
            file.write_all(&body_rlp)?;
        }

        info!(count, path = %path.display(), "block cache saved to disk");
        Ok(())
    }

    /// Load the cache from a file.
    pub fn load_from_file(&self, path: &Path) -> eyre::Result<()> {
        if !path.exists() {
            debug!(path = %path.display(), "no cache file found, starting empty");
            return Ok(());
        }

        let mut file = std::fs::File::open(path)?;

        // Read count
        let mut count_buf = [0u8; 4];
        file.read_exact(&mut count_buf)?;
        let count = u32::from_le_bytes(count_buf) as usize;

        // Clamp to capacity
        let load_count = count.min(self.capacity);
        let mut loaded = 0u32;

        for _ in 0..load_count {
            match Self::read_block_entry(&mut file) {
                Ok(block) => {
                    self.insert(block.hash, block.header, block.body);
                    loaded += 1;
                }
                Err(e) => {
                    warn!(error = %e, "failed to read block entry, stopping load");
                    break;
                }
            }
        }

        info!(loaded, path = %path.display(), "block cache restored from disk");
        Ok(())
    }

    /// Read a single block entry from the file.
    fn read_block_entry(file: &mut std::fs::File) -> eyre::Result<CachedBlock> {
        // Hash
        let mut hash_buf = [0u8; 32];
        file.read_exact(&mut hash_buf)?;
        let hash = B256::from(hash_buf);

        // Header RLP
        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let header_len = u32::from_le_bytes(len_buf) as usize;
        let mut header_rlp = vec![0u8; header_len];
        file.read_exact(&mut header_rlp)?;
        let header = Header::decode(&mut header_rlp.as_slice())?;

        // Body RLP
        file.read_exact(&mut len_buf)?;
        let body_len = u32::from_le_bytes(len_buf) as usize;
        let mut body_rlp = vec![0u8; body_len];
        file.read_exact(&mut body_rlp)?;
        let body = BlockBody::decode(&mut body_rlp.as_slice())?;

        Ok(CachedBlock { hash, header, body })
    }

    /// Extract all blocks from the cache (for saving).
    /// Returns blocks sorted by block number ascending.
    fn drain_blocks(&self) -> Vec<CachedBlock> {
        let inner = self.inner.lock().unwrap();
        let mut blocks: Vec<CachedBlock> = inner
            .hash_by_number
            .iter()
            .filter_map(|(&number, &hash)| {
                let header = inner.headers_by_number.peek(&number)?.clone();
                let body = inner.bodies_by_hash.peek(&hash)?.clone();
                Some(CachedBlock { hash, header, body })
            })
            .collect();
        blocks.sort_by_key(|b| b.header.number);
        blocks
    }
}
