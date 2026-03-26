//! Stateless transaction validator.
//!
//! Performs validation checks that do NOT require chain state:
//! - Signature recovery & validity (already done by Recovered<T>)
//! - Chain ID correctness
//! - Gas limit sanity checks
//! - Max fee / priority fee ordering (EIP-1559)
//! - Nonce is not u64::MAX
//! - Value + gas cost doesn't overflow U256
//! - Signer is not zero address
//! - Reject contract creation transactions (to == None)

use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, U256};
use reth_transaction_pool::{
    validate::ValidTransaction, PoolTransaction, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator,
};
use std::{any::Any, sync::Arc};
use tracing::{debug, warn};

/// Maximum gas limit for a single transaction (30M).
const MAX_TX_GAS_LIMIT: u64 = 30_000_000;

/// Minimum gas limit for any transaction (intrinsic gas for a transfer).
const MIN_TX_GAS_LIMIT: u64 = 21_000;

/// Configuration for the stateless validator.
#[derive(Debug, Clone)]
pub struct StatelessValidatorConfig {
    /// Expected chain ID (1 for mainnet).
    pub chain_id: u64,
    /// Maximum allowed gas limit per transaction.
    pub max_tx_gas_limit: u64,
}

impl Default for StatelessValidatorConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            max_tx_gas_limit: MAX_TX_GAS_LIMIT,
        }
    }
}

/// A transaction validator that performs only stateless checks.
///
/// This validator does NOT access chain state, so it cannot check:
/// - Nonce ordering against account state
/// - Balance sufficiency
/// - Contract code interactions
#[derive(Debug, Clone)]
pub struct StatelessValidator {
    config: Arc<StatelessValidatorConfig>,
}

impl StatelessValidator {
    pub fn new(config: StatelessValidatorConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    fn validate_stateless<T: PoolTransaction>(
        &self,
        tx: &T,
    ) -> Result<(), String> {
        let signer = tx.sender();

        // 0. Reject contract creation transactions
        if tx.is_create() {
            return Err("contract creation transactions are not allowed".to_string());
        }

        // 1. Chain ID check
        if let Some(tx_chain_id) = tx.chain_id() {
            if tx_chain_id != self.config.chain_id {
                return Err(format!(
                    "wrong chain ID: expected {}, got {}",
                    self.config.chain_id, tx_chain_id
                ));
            }
        }

        // 2. Gas limit bounds
        let gas_limit = tx.gas_limit();
        if gas_limit < MIN_TX_GAS_LIMIT {
            return Err(format!("gas limit too low: {} < {}", gas_limit, MIN_TX_GAS_LIMIT));
        }
        if gas_limit > self.config.max_tx_gas_limit {
            return Err(format!(
                "gas limit too high: {} > {}",
                gas_limit, self.config.max_tx_gas_limit
            ));
        }

        // 3. EIP-1559: max_priority_fee <= max_fee
        let max_fee = tx.max_fee_per_gas();
        if let Some(max_priority_fee) = tx.max_priority_fee_per_gas() {
            if max_priority_fee > max_fee {
                return Err(format!(
                    "maxPriorityFeePerGas ({}) > maxFeePerGas ({})",
                    max_priority_fee, max_fee
                ));
            }
        }

        // 4. Gas price must be non-zero
        let gas_price = max_fee;
        if gas_price == 0 {
            return Err("gas price is zero".to_string());
        }

        // 5. Value + gas cost overflow check
        let value = tx.value();
        let max_gas_cost = U256::from(gas_limit) * U256::from(gas_price);
        if value.checked_add(max_gas_cost).is_none() {
            return Err("value + gas cost overflows U256".to_string());
        }

        // 6. Signer must not be zero address
        if signer == Address::ZERO {
            return Err("signer is zero address".to_string());
        }

        // 7. Nonce must not be u64::MAX
        if tx.nonce() == u64::MAX {
            return Err("nonce is u64::MAX".to_string());
        }

        debug!(
            %signer,
            nonce = tx.nonce(),
            gas_limit,
            "transaction passed stateless validation"
        );

        Ok(())
    }
}

impl TransactionValidator for StatelessValidator {
    type Transaction = reth_transaction_pool::EthPooledTransaction;
    type Block = reth_ethereum_primitives::Block;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        match self.validate_stateless(&transaction) {
            Ok(()) => {
                let nonce = transaction.nonce();
                debug!(
                    ?origin,
                    "transaction validated successfully"
                );
                TransactionValidationOutcome::Valid {
                    balance: U256::MAX,
                    state_nonce: nonce,
                    bytecode_hash: None,
                    transaction: ValidTransaction::Valid(transaction),
                    propagate: true,
                    authorities: None,
                }
            }
            Err(reason) => {
                warn!(
                    %reason,
                    "transaction failed stateless validation"
                );
                TransactionValidationOutcome::Invalid(
                    transaction,
                    reth_transaction_pool::error::InvalidPoolTransactionError::Other(
                        Box::new(StatelessValidationError(reason)),
                    ),
                )
            }
        }
    }
}

#[derive(Debug)]
struct StatelessValidationError(String);

impl std::fmt::Display for StatelessValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stateless validation error: {}", self.0)
    }
}

impl std::error::Error for StatelessValidationError {}

impl reth_transaction_pool::error::PoolTransactionError for StatelessValidationError {
    fn is_bad_transaction(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
