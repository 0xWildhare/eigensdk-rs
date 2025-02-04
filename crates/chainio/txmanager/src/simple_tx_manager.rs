use alloy::eips::BlockNumberOrTag;
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::U256;
use alloy::providers::{PendingTransactionBuilder, Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::eth::{TransactionInput, TransactionReceipt, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use backoff::{future::retry, ExponentialBackoffBuilder};
use eigen_logging::logger::SharedLogger;
use eigen_signer::signer::Config;
use reqwest::Url;
use std::time::Duration;
use thiserror::Error;

static FALLBACK_GAS_TIP_CAP: u128 = 5_000_000_000;

pub type Transport = alloy::transports::http::Http<reqwest::Client>;

/// Possible errors raised in Tx Manager
#[derive(Error, Debug, PartialEq)]
pub enum TxManagerError {
    #[error("signer error")]
    SignerError,
    #[error("send error")]
    SendTxError,
    #[error("wait_for_receipt error")]
    WaitForReceiptError,
    #[error("address error")]
    AddressError,
    #[error("invalid url error")]
    InvalidUrlError,
}

/// A simple transaction manager that encapsulates operations to send transactions to an Ethereum node.
pub struct SimpleTxManager {
    logger: SharedLogger,
    gas_limit_multiplier: f64,
    private_key: String,
    provider: RootProvider<Transport>,
}

impl SimpleTxManager {
    /// Creates a new SimpleTxManager.
    ///
    /// # Arguments
    ///
    /// * `logger`: The logger to be used.
    /// * `gas_limit_multiplier`: The gas limit multiplier.
    /// * `private_key`: The private key of the wallet.
    /// * `rpc_url`: The RPC URL. It could be an anvil node or any other node.
    ///
    /// # Returns
    ///
    /// * The SimpleTxManager created.
    ///
    /// # Errors
    ///
    /// * If the URL is invalid.
    pub fn new(
        logger: SharedLogger,
        gas_limit_multiplier: f64,
        private_key: &str,
        rpc_url: &str,
    ) -> Result<SimpleTxManager, TxManagerError> {
        let url = Url::parse(rpc_url)
            .inspect_err(|err| logger.error("Failed to parse url", &err.to_string()))
            .map_err(|_| TxManagerError::InvalidUrlError)?;
        let provider = ProviderBuilder::new().on_http(url);
        Ok(SimpleTxManager {
            logger,
            gas_limit_multiplier,
            private_key: private_key.to_string(),
            provider,
        })
    }

    /// Sets the gas limit multiplier.
    ///
    /// # Arguments
    ///
    /// * `multiplier` - The gas limit multiplier.
    pub fn with_gas_limit_multiplier(&mut self, multiplier: f64) {
        self.gas_limit_multiplier = multiplier;
    }

    /// Creates a local signer.
    ///
    /// # Returns
    ///
    /// * `PrivateKeySigner` The local signer.
    ///
    /// # Errors
    ///
    /// * If the signer cannot be created.
    fn create_local_signer(&self) -> Result<PrivateKeySigner, TxManagerError> {
        let config = Config::PrivateKey(self.private_key.clone());
        Config::signer_from_config(config)
            .inspect_err(|err| {
                self.logger
                    .error("Failed to create signer", &err.to_string())
            })
            .map_err(|_| TxManagerError::SignerError)
    }

    /// Send is used to send a transaction to the Ethereum node. It takes an unsigned/signed transaction,
    /// sends it to the Ethereum node and waits for the receipt.
    /// If you pass in a signed transaction it will ignore the signature
    /// and re-sign the transaction after adding the nonce and gas limit.
    ///
    /// # Arguments
    ///
    /// * `tx`: The transaction to be sent.
    ///
    /// # Returns
    ///
    /// * `TransactionReceipt` The transaction receipt.
    ///
    /// # Errors
    ///
    /// * `TxManagerError` - If the transaction cannot be sent, or there is an error
    ///   signing the transaction or estimating gas and nonce.
    pub async fn send_tx(
        &self,
        tx: &mut TransactionRequest,
    ) -> Result<TransactionReceipt, TxManagerError> {
        // Estimating gas and nonce
        self.logger.debug("Estimating gas and nonce", "");

        let tx = self.estimate_gas_and_nonce(tx).await.inspect_err(|err| {
            self.logger
                .error("Failed to estimate gas", &err.to_string())
        })?;

        let signer = self.create_local_signer()?;
        let wallet = EthereumWallet::from(signer);

        let signed_tx = tx
            .build(&wallet)
            .await
            .inspect_err(|err| {
                self.logger
                    .error("Failed to build and sign transaction", &err.to_string())
            })
            .map_err(|_| TxManagerError::SendTxError)?;

        // send transaction and get receipt
        let pending_tx = self
            .provider
            .send_tx_envelope(signed_tx)
            .await
            .inspect_err(|err| self.logger.error("Failed to get receipt", &err.to_string()))
            .map_err(|_| TxManagerError::SendTxError)?;

        self.logger.debug(
            "Transaction sent. Pending transaction: ",
            &pending_tx.tx_hash().to_string(),
        );
        // wait for the transaction to be mined
        SimpleTxManager::wait_for_receipt(self, pending_tx).await
    }

    /// Send a transaction to the Ethereum node. It takes an unsigned/signed transaction,
    /// sends it to the Ethereum node and waits for the receipt.
    /// If you pass in a signed transaction it will ignore the signature
    /// and re-sign the transaction after adding the nonce and gas limit.
    /// If the transaction fails, it will retry sending the transaction until it gets a receipt,
    /// using an **exponential backoff** strategy.
    /// If no receipt is received after `max_elapsed_time`, it will return an error.
    ///
    /// # Arguments
    ///
    /// * `tx`: The transaction to be sent.
    /// * `initial_interval`: The initial interval duration for the backoff.
    /// * `max_elapsed_time`: The maximum elapsed time for retrying.
    /// * `multiplier`: The multiplier used to compute the exponential backoff.
    ///
    /// # Returns
    ///
    /// * `TransactionReceipt` The transaction receipt.
    ///
    /// # Errors
    ///
    /// * `TxManagerError` - If the transaction cannot be sent, or there is an error
    ///   signing the transaction or estimating gas and nonce.
    pub async fn send_tx_with_retries(
        &self,
        tx: &mut TransactionRequest,
        initial_interval: Duration,
        max_elapsed_time: Duration,
        multiplier: f64,
    ) -> Result<TransactionReceipt, TxManagerError> {
        let backoff_config = ExponentialBackoffBuilder::default()
            .with_initial_interval(initial_interval)
            .with_max_elapsed_time(Some(max_elapsed_time))
            .with_multiplier(multiplier)
            .build();
        retry(backoff_config, || async {
            let mut cloned_tx = tx.clone();
            Ok(self.send_tx(&mut cloned_tx).await?)
        })
        .await
    }

    /// Estimates the gas and nonce for a transaction.
    ///
    /// # Arguments
    ///
    /// * `tx`: The transaction for which we want to estimate the gas and nonce.
    ///
    /// # Returns
    ///
    /// * The transaction request with the gas and nonce estimated.
    ///
    /// # Errors
    ///
    /// * If the transaction request could not sent of gives an error.
    /// * If the latest block header could not be retrieved.
    /// * If the gas price could not be estimated.
    /// * If the gas limit could not be estimated.
    /// * If the destination address could not be retrieved.
    async fn estimate_gas_and_nonce(
        &self,
        tx: &TransactionRequest,
    ) -> Result<TransactionRequest, TxManagerError> {
        let gas_tip_cap = self.provider.get_max_priority_fee_per_gas().await
        .inspect_err(|err|
            self.logger.info("eth_maxPriorityFeePerGas is unsupported by current backend, using fallback gasTipCap",
            &err.to_string()))
        .unwrap_or(FALLBACK_GAS_TIP_CAP);

        let header = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest, false.into())
            .await
            .ok()
            .flatten()
            .map(|block| block.header)
            .ok_or(TxManagerError::SendTxError)
            .inspect_err(|_| self.logger.error("Failed to get latest block header", ""))?;

        // 2*baseFee + gas_tip_cap makes sure that the tx remains includeable for 6 consecutive 100% full blocks.
        // see https://www.blocknative.com/blog/eip-1559-fees
        let base_fee = header.base_fee_per_gas.ok_or(TxManagerError::SendTxError)?;
        let gas_fee_cap: u128 = (2 * base_fee + U256::from(gas_tip_cap).to::<u64>()).into();

        let mut gas_limit = tx.gas_limit();
        let tx_input = tx.input().unwrap_or_default().to_vec();
        // we only estimate if gas_limit is not already set
        if let Some(0) = gas_limit {
            let from = self.create_local_signer()?.address();
            let to = tx.to().ok_or(TxManagerError::SendTxError)?;

            let mut tx_request = TransactionRequest::default()
                .to(to)
                .from(from)
                .value(tx.value().unwrap_or_default())
                .input(TransactionInput::new(tx_input.clone().into()));
            tx_request.set_max_priority_fee_per_gas(gas_tip_cap);
            tx_request.set_max_fee_per_gas(gas_fee_cap);

            gas_limit = Some(
                self.provider
                    .estimate_gas(&tx_request)
                    .await
                    .map_err(|_| TxManagerError::SendTxError)?,
            );
        }
        let gas_price_multiplied =
            tx.gas_price().unwrap_or_default() as f64 * self.gas_limit_multiplier;
        let gas_price = gas_price_multiplied as u128;

        let to = tx.to().ok_or(TxManagerError::SendTxError)?;

        let new_tx = TransactionRequest::default()
            .with_to(to)
            .with_value(tx.value().unwrap_or_default())
            .with_gas_limit(gas_limit.unwrap_or_default())
            .with_nonce(tx.nonce().unwrap_or_default())
            .with_input(tx_input)
            .with_chain_id(tx.chain_id().unwrap_or(1))
            .with_max_priority_fee_per_gas(gas_tip_cap)
            .with_max_fee_per_gas(gas_fee_cap)
            .with_gas_price(gas_price);

        Ok(new_tx)
    }

    /// Waits for the transaction receipt.
    ///
    /// This is a wrapper around `PendingTransactionBuilder::get_receipt`.
    ///
    /// # Arguments
    ///
    /// * `pending_tx`: The pending transaction builder we want to wait for.
    ///
    /// # Returns
    ///
    /// * The block number in which the transaction was included.
    /// * `None` if the transaction was not included in a block or an error ocurred.
    ///
    /// # Errors
    ///
    /// * `TxManagerError` - If the transaction receipt cannot be retrieved.
    pub async fn wait_for_receipt(
        &self,
        pending_tx: PendingTransactionBuilder<Transport, Ethereum>,
    ) -> Result<TransactionReceipt, TxManagerError> {
        pending_tx
            .get_receipt()
            .await
            .inspect_err(|err| self.logger.error("Failed to get receipt", &err.to_string()))
            .map_err(|_| TxManagerError::WaitForReceiptError)
    }
}

#[cfg(test)]
mod tests {
    use super::{SimpleTxManager, TxManagerError};
    use alloy::consensus::TxLegacy;
    use alloy::network::TransactionBuilder;
    use alloy::primitives::{address, bytes, TxKind::Call, U256};
    use alloy::rpc::types::eth::TransactionRequest;
    use eigen_logging::get_test_logger;
    use eigen_testing_utils::anvil::start_anvil_container;
    use std::time::Duration;
    use tokio;
    use tokio::time::Instant;

    #[tokio::test]
    async fn test_send_transaction_from_legacy() {
        let (_container, rpc_url, _ws_endpoint) = start_anvil_container().await;
        let logger = get_test_logger();

        let private_key =
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string();
        let simple_tx_manager =
            SimpleTxManager::new(logger, 1.0, private_key.as_str(), rpc_url.as_str()).unwrap();
        let to = address!("a0Ee7A142d267C1f36714E4a8F75612F20a79720");

        let account_nonce = 0x69; // nonce queried from the sender account
        let tx = TxLegacy {
            to: Call(to),
            value: U256::from(1_000_000_000),
            gas_limit: 2_000_000,
            nonce: account_nonce,
            gas_price: 21_000_000_000,
            input: bytes!(),
            chain_id: Some(31337),
        };
        let mut tx_request: TransactionRequest = tx.into();

        // send transaction and wait for receipt
        let receipt = simple_tx_manager.send_tx(&mut tx_request).await.unwrap();
        let block_number = receipt.block_number.unwrap();
        println!("Transaction mined in block: {}", block_number);
        assert!(block_number > 0);
        assert_eq!(receipt.to, Some(to));
    }

    #[tokio::test]
    async fn test_send_transaction_from_eip1559() {
        let (_container, rpc_url, _ws_endpoint) = start_anvil_container().await;
        let logger = get_test_logger();

        let private_key =
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string();
        let simple_tx_manager =
            SimpleTxManager::new(logger, 1.0, private_key.as_str(), rpc_url.as_str()).unwrap();
        let to = address!("a0Ee7A142d267C1f36714E4a8F75612F20a79720");

        let account_nonce = 0x69; // nonce queried from the sender account
        let mut tx = TransactionRequest::default()
            .with_to(to)
            .with_nonce(account_nonce)
            .with_chain_id(31337)
            .with_value(U256::from(100))
            .with_gas_limit(21_000)
            .with_max_priority_fee_per_gas(1_000_000_000)
            .with_max_fee_per_gas(20_000_000_000);
        tx.set_gas_price(21_000_000_000);

        // send transaction and wait for receipt
        let receipt = simple_tx_manager.send_tx(&mut tx).await.unwrap();
        let block_number = receipt.block_number.unwrap();
        println!("Transaction mined in block: {}", block_number);
        assert!(block_number > 0);
        assert_eq!(receipt.to, Some(to));
    }

    #[tokio::test]
    async fn test_send_transaction_with_retries_returns_after_timeout() {
        let rpc_url = "http://fake:8545";
        let logger = get_test_logger();
        let private_key =
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string();
        let simple_tx_manager =
            SimpleTxManager::new(logger, 1.0, private_key.as_str(), rpc_url).unwrap();
        let to = address!("a0Ee7A142d267C1f36714E4a8F75612F20a79720");

        let account_nonce = 0x69;
        let mut tx = TransactionRequest::default()
            .with_to(to)
            .with_nonce(account_nonce)
            .with_chain_id(31337)
            .with_value(U256::from(100))
            .with_gas_limit(21_000)
            .with_max_priority_fee_per_gas(1_000_000_000)
            .with_max_fee_per_gas(20_000_000_000)
            .with_gas_price(21_000_000_000);
        let start = Instant::now();

        let result = simple_tx_manager
            .send_tx_with_retries(
                &mut tx,
                Duration::from_millis(5),
                Duration::from_secs(1),
                1.0,
            )
            .await;
        assert_eq!(result, Err(TxManagerError::SendTxError));
        // substract one interval for asserting, because if the last try does not fit in the max_elapsed_time, it will not be executed
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_secs(1) - Duration::from_millis(5));
    }
}
