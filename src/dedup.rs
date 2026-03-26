//! Transaction deduplication using an LRU cache.
//!
//! Multiple peers often gossip the same transaction. This module tracks
//! recently seen tx hashes and skips duplicates before forwarding.

use alloy_primitives::B256;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tracing::info;

/// Deduplication statistics.
#[derive(Debug)]
struct DedupStats {
    total: AtomicU64,
    duplicates: AtomicU64,
    unique: AtomicU64,
}

/// Configuration for the dedup cache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DedupConfig {
    /// Maximum number of tx hashes to track.
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,
    /// How often to log dedup statistics (in number of transactions).
    #[serde(default = "default_log_interval")]
    pub log_interval: u64,
}

fn default_cache_size() -> usize {
    100_000
}

fn default_log_interval() -> u64 {
    1000
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            cache_size: default_cache_size(),
            log_interval: default_log_interval(),
        }
    }
}

/// LRU-based transaction deduplicator.
pub struct TxDedup {
    cache: Mutex<LruCache<B256, ()>>,
    stats: DedupStats,
    log_interval: u64,
}

impl TxDedup {
    pub fn new(config: DedupConfig) -> Self {
        let cap = NonZeroUsize::new(config.cache_size).expect("cache_size must be > 0");
        Self {
            cache: Mutex::new(LruCache::new(cap)),
            stats: DedupStats {
                total: AtomicU64::new(0),
                duplicates: AtomicU64::new(0),
                unique: AtomicU64::new(0),
            },
            log_interval: config.log_interval,
        }
    }

    /// Returns `true` if this tx hash has NOT been seen before (i.e., is new).
    /// Returns `false` if it's a duplicate.
    pub fn check_and_insert(&self, hash: B256) -> bool {
        let total = self.stats.total.fetch_add(1, Ordering::Relaxed) + 1;

        let is_new = {
            let mut cache = self.cache.lock().unwrap();
            if cache.contains(&hash) {
                // Already seen - mark as recently used
                cache.promote(&hash);
                false
            } else {
                cache.push(hash, ());
                true
            }
        };

        if is_new {
            self.stats.unique.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.duplicates.fetch_add(1, Ordering::Relaxed);
        }

        // Periodically log stats
        if self.log_interval > 0 && total % self.log_interval == 0 {
            let unique = self.stats.unique.load(Ordering::Relaxed);
            let duplicates = self.stats.duplicates.load(Ordering::Relaxed);
            let dup_rate = if total > 0 {
                (duplicates as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            info!(
                total,
                unique,
                duplicates,
                dup_rate = format!("{:.1}%", dup_rate),
                "dedup stats"
            );
        }

        is_new
    }
}
