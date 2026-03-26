//! P2P network setup for the sentry node.
//!
//! Sets up the devp2p networking layer using reth components:
//! - Discv4 for peer discovery
//! - RLPx for encrypted communication
//! - eth protocol for transaction gossip
//! - NewBlock caching for serving peer requests
//!
//! Block sync is disabled - we cache NewBlock announcements and use them
//! to respond to peer requests.

use crate::block_cache::BlockCache;
use crate::block_import::CachingBlockImport;
use crate::eth_proxy;
use crate::forwarder::{ForwardableTx, TxForwarder};
use reth_chainspec::MAINNET;
use reth_eth_wire::EthNetworkPrimitives;
use reth_network::{config::SecretKey, NetworkConfigBuilder, NetworkManager};
use reth_network_api::{
    events::PeerEvent, NetworkEvent, NetworkEventListenerProvider, PeersInfo,
};
use reth_transaction_pool::{
    blobstore::InMemoryBlobStore, CoinbaseTipOrdering, EthPooledTransaction, Pool,
    PoolTransaction, TransactionPool,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tracing::{debug, info};

use crate::validator::{StatelessValidator, StatelessValidatorConfig};

/// Configuration for the sentry network.
#[derive(Debug, Clone)]
pub struct SentryNetworkConfig {
    /// Chain ID (1 = mainnet).
    pub chain_id: u64,
    /// Maximum number of peers.
    pub max_peers: u32,
    /// Port to listen on for P2P.
    pub p2p_port: u16,
    /// Port for discovery (UDP).
    pub discovery_port: u16,
}

impl Default for SentryNetworkConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            max_peers: 50,
            p2p_port: 30303,
            discovery_port: 30303,
        }
    }
}

/// Type alias for our transaction pool.
pub type SentryTxPool =
    Pool<StatelessValidator, CoinbaseTipOrdering<EthPooledTransaction>, InMemoryBlobStore>;

/// Start the sentry P2P network.
///
/// Caches NewBlock announcements from peers and uses them to respond
/// to GetBlockHeaders/GetBlockBodies requests.
pub async fn start_sentry_network(
    net_config: SentryNetworkConfig,
    forwarder: Arc<TxForwarder>,
    secret_key: SecretKey,
) -> eyre::Result<()> {
    info!("starting sentry node on port {}", net_config.p2p_port);

    // Create the stateless validator
    let validator_config = StatelessValidatorConfig {
        chain_id: net_config.chain_id,
        ..Default::default()
    };
    let validator = StatelessValidator::new(validator_config);

    // Create the transaction pool with our stateless validator
    let blob_store = InMemoryBlobStore::default();
    let tx_pool: SentryTxPool = Pool::new(
        validator,
        CoinbaseTipOrdering::default(),
        blob_store,
        Default::default(),
    );

    info!("transaction pool created");

    // Create block cache and custom block import
    let block_cache = BlockCache::new(256);
    let block_import = CachingBlockImport::new(block_cache.clone());

    // Build network config using noop provider (no block sync)
    let peers_config = reth_network::PeersConfig::default()
        .with_max_outbound(net_config.max_peers as usize / 2)
        .with_max_inbound_opt(Some(net_config.max_peers as usize / 2));

    let net_builder = NetworkConfigBuilder::<EthNetworkPrimitives>::new(secret_key)
        .listener_port(net_config.p2p_port)
        .discovery_port(net_config.discovery_port)
        .peer_config(peers_config)
        .block_import(Box::new(block_import))
        .disable_dns_discovery()
        .mainnet_boot_nodes();

    let network_config = net_builder.build_with_noop_provider(MAINNET.clone());

    // Build the network
    let mut network = NetworkManager::new(network_config).await?;

    // Wire up the ETH request handler channel
    let (eth_req_tx, eth_req_rx) = mpsc::channel(256);
    network.set_eth_request_handler(eth_req_tx);

    // Build with transactions manager
    let builder = network
        .into_builder()
        .transactions(tx_pool.clone(), Default::default());

    let (network_handle, network, transactions, _) = builder.split_with_handle();

    info!(
        peer_id = %network_handle.peer_id(),
        "sentry node started, listening for peers"
    );

    // Spawn network manager and transaction manager as background tasks
    tokio::spawn(network);
    tokio::spawn(transactions);

    // Spawn ETH request handler (uses block cache)
    tokio::spawn(eth_proxy::start_eth_request_handler(eth_req_rx, block_cache));

    // Subscribe to network events
    let mut events = network_handle.event_listener();

    // Spawn a task to monitor peer connections
    let handle_clone = network_handle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let num_peers = handle_clone.num_connected_peers();
            info!(num_peers, "peer count update");
        }
    });

    // Spawn a task to listen for new transactions in the pool and forward them
    let pool_clone = tx_pool.clone();
    tokio::spawn(async move {
        let mut pending_txs = pool_clone.pending_transactions_listener();
        info!("listening for new pending transactions to forward");

        while let Some(tx_hash) = pending_txs.recv().await {
            debug!(%tx_hash, "new pending transaction detected");

            // Get the full transaction from the pool and encode it
            if let Some(tx) = pool_clone.get(&tx_hash) {
                let consensus_tx = tx.transaction.clone_into_consensus();
                let mut raw_tx = Vec::new();
                    alloy_rlp::Encodable::encode(consensus_tx.inner(), &mut raw_tx);

                forwarder
                    .forward(ForwardableTx {
                        hash: tx_hash,
                        raw_tx,
                    })
                    .await;
            }
        }
    });

    // Main event loop - handle network events
    while let Some(event) = events.next().await {
        match event {
            NetworkEvent::ActivePeerSession { info, .. } => {
                info!(
                    peer_id = %info.peer_id,
                    remote_addr = %info.remote_addr,
                    client_version = %info.client_version,
                    "new peer connected"
                );
            }
            NetworkEvent::Peer(peer_event) => match peer_event {
                PeerEvent::SessionClosed { peer_id, reason } => {
                    debug!(
                        %peer_id,
                        ?reason,
                        "peer disconnected"
                    );
                }
                PeerEvent::PeerAdded(peer_id) => {
                    debug!(%peer_id, "peer added to pool");
                }
                PeerEvent::PeerRemoved(peer_id) => {
                    debug!(%peer_id, "peer removed from pool");
                }
                PeerEvent::SessionEstablished(_) => {}
            },
        }
    }

    Ok(())
}
