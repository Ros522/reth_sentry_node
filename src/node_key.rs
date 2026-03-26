//! Node key persistence.
//!
//! Loads or generates a secp256k1 secret key for the node's P2P identity.
//! The key is stored in a file so the node keeps the same peer ID across restarts.

use reth_network::config::rng_secret_key;
use secp256k1::SecretKey;
use std::fs;
use std::path::Path;
use tracing::info;

/// Load a node key from file, or generate and save a new one.
pub fn load_or_generate(path: &Path) -> eyre::Result<SecretKey> {
    if path.exists() {
        let hex = fs::read_to_string(path)?;
        let bytes = alloy_primitives::hex::decode(hex.trim())?;
        let key = SecretKey::from_slice(&bytes)?;
        info!(path = %path.display(), "loaded existing node key");
        Ok(key)
    } else {
        let key = rng_secret_key();
        let hex = alloy_primitives::hex::encode(key.secret_bytes());

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &hex)?;
        info!(path = %path.display(), "generated new node key");
        Ok(key)
    }
}
