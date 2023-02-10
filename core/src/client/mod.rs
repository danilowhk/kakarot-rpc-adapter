use eyre::Result;
use jsonrpsee::types::error::CallError;
use std::convert::From;

use reth_primitives::{
    rpc::{BlockId, BlockNumber, H256},
    Address, Bytes, H160, H256 as PrimitiveH256, U256, U64,
};
use reth_rpc_types::{SyncInfo, SyncStatus, TransactionReceipt};
use starknet::{
    core::types::FieldElement,
    providers::jsonrpc::{
        models::{
            BlockId as StarknetBlockId, BlockTag, BroadcastedInvokeTransaction,
            BroadcastedInvokeTransactionV1, DeployAccountTransactionReceipt,
            DeployTransactionReceipt, FunctionCall, InvokeTransaction, InvokeTransactionReceipt,
            MaybePendingBlockWithTxHashes, MaybePendingBlockWithTxs,
            MaybePendingTransactionReceipt, SyncStatusType, Transaction as StarknetTransaction,
            TransactionReceipt as StarknetTransactionReceipt,
            TransactionStatus as StarknetTransactionStatus,
        },
        HttpTransport, JsonRpcClient, JsonRpcClientError,
    },
};

use thiserror::Error;
use url::Url;
extern crate hex;

use crate::helpers::{
    create_default_transaction_receipt, decode_execute_at_address_return,
    ethers_block_id_to_starknet_block_id, felt_option_to_u256, felt_to_u256, hash_to_field_element,
    starknet_address_to_ethereum_address, vec_felt_to_bytes, FeltOrFeltArray,
    MaybePendingStarknetBlock,
};

use reth_primitives::{Bloom, H64};
use std::collections::BTreeMap;

use crate::client::{
    constants::{
        selectors::EXECUTE_AT_ADDRESS, CHAIN_ID, KAKAROT_CONTRACT_ACCOUNT_CLASS_HASH,
        KAKAROT_MAIN_CONTRACT_ADDRESS,
    },
    types::{Block, BlockTransactions, Header, Rich, RichBlock, Transaction as EtherTransaction},
};
use async_trait::async_trait;
use mockall::predicate::*;
use mockall::*;
use reth_rpc_types::Index;
pub mod constants;
use constants::selectors::BYTECODE;
pub mod types;

use self::constants::selectors::{COMPUTE_STARKNET_ADDRESS, GET_EVM_ADDRESS};

#[derive(Debug, Error)]
pub enum KakarotClientError {
    #[error(transparent)]
    RequestError(#[from] JsonRpcClientError<reqwest::Error>),
    #[error(transparent)]
    OtherError(#[from] anyhow::Error),
}

#[automock]
#[async_trait]
pub trait StarknetClient: Send + Sync {
    async fn block_number(&self) -> Result<U256, KakarotClientError>;

    async fn get_eth_block_from_starknet_block(
        &self,
        block_id: StarknetBlockId,
        hydrated_tx: bool,
    ) -> Result<RichBlock, KakarotClientError>;

    async fn get_code(
        &self,
        ethereum_address: Address,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Bytes, KakarotClientError>;

    async fn call_view(
        &self,
        ethereum_address: Address,
        calldata: Bytes,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Bytes, KakarotClientError>;
    async fn transaction_by_block_number_and_index(
        &self,
        block_id: StarknetBlockId,
        tx_index: Index,
    ) -> Result<EtherTransaction, KakarotClientError>;
    async fn syncing(&self) -> Result<SyncStatus, KakarotClientError>;
    async fn block_transaction_count_by_number(
        &self,
        number: BlockNumber,
    ) -> Result<Option<U256>, KakarotClientError>;
    async fn block_transaction_count_by_hash(
        &self,
        hash: H256,
    ) -> Result<Option<U256>, KakarotClientError>;
    async fn compute_starknet_address(
        &self,
        ethereum_address: Address,
        starknet_block_id: StarknetBlockId,
    ) -> Result<FieldElement, KakarotClientError>;
    async fn submit_starknet_transaction(
        &self,
        max_fee: FieldElement,
        signature: Vec<FieldElement>,
        nonce: FieldElement,
        sender_address: FieldElement,
        calldata: Vec<FieldElement>,
    ) -> Result<H256, KakarotClientError>;
    async fn get_transaction_receipt(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionReceipt>, KakarotClientError>;

    async fn get_evm_address(
        &self,
        starknet_address: FieldElement,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Address, KakarotClientError>;
    async fn starknet_tx_into_eth_tx(
        &self,
        tx: StarknetTransaction,
        block_hash: Option<PrimitiveH256>,
        block_number: Option<U256>,
    ) -> Result<EtherTransaction, KakarotClientError>;
    async fn starknet_block_to_eth_block(
        &self,
        block: MaybePendingStarknetBlock,
    ) -> Result<RichBlock, KakarotClientError>;
    async fn filter_transactions(
        &self,
        initial_transactions: Vec<StarknetTransaction>,
        blockhash_opt: Option<PrimitiveH256>,
        blocknum_opt: Option<U256>,
    ) -> Result<BlockTransactions, KakarotClientError>;
}
pub struct StarknetClientImpl {
    client: JsonRpcClient<HttpTransport>,
    kakarot_main_contract: FieldElement,
}

impl From<KakarotClientError> for jsonrpsee::core::Error {
    fn from(err: KakarotClientError) -> Self {
        match err {
            KakarotClientError::RequestError(e) => jsonrpsee::core::Error::Call(CallError::Failed(
                anyhow::anyhow!("Kakarot Core: Light Client Request Error: {}", e),
            )),
            KakarotClientError::OtherError(e) => jsonrpsee::core::Error::Call(CallError::Failed(e)),
        }
    }
}

impl StarknetClientImpl {
    pub fn new(starknet_rpc: &str) -> Result<Self> {
        let url = Url::parse(starknet_rpc)?;
        let kakarot_main_contract = FieldElement::from_hex_be(KAKAROT_MAIN_CONTRACT_ADDRESS)?;
        Ok(Self {
            client: JsonRpcClient::new(HttpTransport::new(url)),
            kakarot_main_contract,
        })
    }

    /// Get the Ethereum address of a Starknet Kakarot smart-contract by calling get_evm_address on it.
    /// If the contract's get_evm_address errors, returns the Starknet address sliced to 20 bytes to conform with EVM addresses formats.
    ///
    /// ## Arguments
    ///
    /// * `starknet_address` - The Starknet address of the contract.
    /// * `starknet_block_id` - The block id to query the contract at.
    ///
    /// ## Returns
    ///
    /// * `eth_address` - The Ethereum address of the contract.
    pub async fn safe_get_evm_address(
        &self,
        starknet_address: FieldElement,
        starknet_block_id: StarknetBlockId,
    ) -> Address {
        let eth_address = self
            .get_evm_address(starknet_address, starknet_block_id)
            .await
            .unwrap_or_else(|_| starknet_address_to_ethereum_address(starknet_address));
        eth_address
    }
}

#[async_trait]
impl StarknetClient for StarknetClientImpl {
    /// Get the number of transactions in a block given a block id.
    /// The number of transactions in a block.
    ///
    /// ## Arguments
    ///
    ///
    ///
    /// ## Returns
    ///
    ///  * `block_number(u64)` - The block number.
    ///
    /// `Ok(ContractClass)` if the operation was successful.
    /// `Err(KakarotClientError)` if the operation failed.
    async fn block_number(&self) -> Result<U256, KakarotClientError> {
        let block_number = self.client.block_number().await?;
        Ok(U256::from(block_number))
    }

    /// Get the block given a block id.
    /// The block.
    /// ## Arguments
    /// * `block_id(StarknetBlockId)` - The block id.
    /// * `hydrated_tx(bool)` - Whether to hydrate the transactions.
    /// ## Returns
    /// `Ok(RichBlock)` if the operation was successful.
    /// `Err(KakarotClientError)` if the operation failed.
    async fn get_eth_block_from_starknet_block(
        &self,
        block_id: StarknetBlockId,
        hydrated_tx: bool,
    ) -> Result<RichBlock, KakarotClientError> {
        // let hydrated_tx = false;
        let starknet_block = if hydrated_tx {
            MaybePendingStarknetBlock::BlockWithTxs(
                self.client.get_block_with_txs(&block_id).await?,
            )
        } else {
            MaybePendingStarknetBlock::BlockWithTxHashes(
                self.client.get_block_with_tx_hashes(&block_id).await?,
            )
        };
        // fetch gas limit, public key, and nonce from starknet rpc

        let block = self
            .starknet_block_to_eth_block(starknet_block)
            .await
            .unwrap();
        Ok(block)
    }

    /// Get the number of transactions in a block given a block id.
    /// The number of transactions in a block.
    ///
    /// ## Arguments
    ///
    ///
    ///
    /// ## Returns
    ///
    ///  * `block_number(u64)` - The block number.
    ///
    /// `Ok(Bytes)` if the operation was successful.
    /// `Err(KakarotClientError)` if the operation failed.
    async fn get_code(
        &self,
        ethereum_address: Address,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Bytes, KakarotClientError> {
        // Convert the Ethereum address to a hex-encoded string
        let address_hex = hex::encode(ethereum_address);
        // Convert the hex-encoded string to a FieldElement
        let ethereum_address_felt = FieldElement::from_hex_be(&address_hex).map_err(|e| {
            KakarotClientError::OtherError(anyhow::anyhow!(
                "Kakarot Core: Failed to convert Ethereum address to FieldElement: {}",
                e
            ))
        })?;

        // Prepare the calldata for the get_starknet_contract_address function call
        let tx_calldata_vec = vec![ethereum_address_felt];
        let request = FunctionCall {
            contract_address: self.kakarot_main_contract,
            entry_point_selector: COMPUTE_STARKNET_ADDRESS,
            calldata: tx_calldata_vec,
        };
        // Make the function call to get the Starknet contract address
        let starknet_contract_address = self.client.call(request, &starknet_block_id).await?;
        // Concatenate the result of the function call
        let concatenated_result = starknet_contract_address
            .into_iter()
            .fold(FieldElement::ZERO, |acc, x| acc + x);

        // Prepare the calldata for the bytecode function call
        let request = FunctionCall {
            contract_address: concatenated_result,
            entry_point_selector: BYTECODE,
            calldata: vec![],
        };
        // Make the function call to get the contract bytecode
        let contract_bytecode = self.client.call(request, &starknet_block_id).await?;
        // Convert the result of the function call to a vector of bytes
        let contract_bytecode_in_u8: Vec<u8> = contract_bytecode
            .into_iter()
            .flat_map(|x| x.to_bytes_be())
            .collect();
        let bytes_result = Bytes::from(contract_bytecode_in_u8);
        Ok(bytes_result)
    }
    // Return the bytecode as a Result<Bytes>
    async fn call_view(
        &self,
        ethereum_address: Address,
        calldata: Bytes,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Bytes, KakarotClientError> {
        let address_hex = hex::encode(ethereum_address);

        let ethereum_address_felt = FieldElement::from_hex_be(&address_hex).map_err(|e| {
            KakarotClientError::OtherError(anyhow::anyhow!(
                "Kakarot Core: Failed to convert Ethereum address to FieldElement: {}",
                e
            ))
        })?;

        let mut calldata_vec = calldata
            .clone()
            .into_iter()
            .map(FieldElement::from)
            .collect::<Vec<FieldElement>>();

        let mut call_parameters = vec![
            ethereum_address_felt,
            FieldElement::ZERO,
            FieldElement::MAX,
            calldata.len().into(),
        ];

        call_parameters.append(&mut calldata_vec);

        let request = FunctionCall {
            contract_address: self.kakarot_main_contract,
            entry_point_selector: EXECUTE_AT_ADDRESS,
            calldata: call_parameters,
        };

        let call_result: Vec<FieldElement> = self.client.call(request, &starknet_block_id).await?;

        // Parse and decode Kakarot's call return data (temporary solution and not scalable - will
        // fail is Kakarot API changes)
        // Declare Vec of Result
        // TODO: Change to decode based on ABI or use starknet-rs future feature to decode return
        // params
        let segmented_result = decode_execute_at_address_return(call_result)?;

        // Convert the result of the function call to a vector of bytes
        let return_data = segmented_result.get(6).ok_or_else(|| {
            KakarotClientError::OtherError(anyhow::anyhow!(
                "Cannot parse and decode last argument of Kakarot call",
            ))
        })?;
        if let FeltOrFeltArray::FeltArray(felt_array) = return_data {
            let result: Vec<u8> = felt_array
                .iter()
                .map(|x| x.to_string())
                .filter_map(|s| s.parse().ok())
                .collect();
            let bytes_result = Bytes::from(result);
            return Ok(bytes_result);
        }
        Err(KakarotClientError::OtherError(anyhow::anyhow!(
            "Cannot parse and decode the return data of Kakarot call"
        )))
    }

    /// Get the syncing status of the light client
    /// # Arguments
    /// # Returns
    ///  `Ok(SyncStatus)` if the operation was successful.
    ///  `Err(KakarotClientError)` if the operation failed.
    async fn syncing(&self) -> Result<SyncStatus, KakarotClientError> {
        let status = self.client.syncing().await?;

        match status {
            SyncStatusType::NotSyncing => Ok(SyncStatus::None),

            SyncStatusType::Syncing(data) => {
                let starting_block: U256 = U256::from(data.starting_block_num);
                let current_block: U256 = U256::from(data.current_block_num);
                let highest_block: U256 = U256::from(data.highest_block_num);
                let warp_chunks_amount: Option<U256> = None;
                let warp_chunks_processed: Option<U256> = None;

                let status_info = SyncInfo {
                    starting_block,
                    current_block,
                    highest_block,
                    warp_chunks_amount,
                    warp_chunks_processed,
                };

                Ok(SyncStatus::Info(status_info))
            }
        }
    }

    /// Get the number of transactions in a block given a block number.
    /// The number of transactions in a block.
    ///
    /// # Arguments
    ///
    /// * `number(u64)` - The block number.
    ///
    /// # Returns
    ///
    ///  * `transaction_count(U256)` - The number of transactions.
    ///
    /// `Ok(Option<U256>)` if the operation was successful.
    /// `Err(KakarotClientError)` if the operation failed.
    async fn block_transaction_count_by_number(
        &self,
        number: BlockNumber,
    ) -> Result<Option<U256>, KakarotClientError> {
        let starknet_block_id = ethers_block_id_to_starknet_block_id(BlockId::Number(number))?;
        let starknet_block = self
            .client
            .get_block_with_tx_hashes(&starknet_block_id)
            .await?;
        match starknet_block {
            MaybePendingBlockWithTxHashes::Block(block) => {
                Ok(Some(U256::from(block.transactions.len())))
            }
            MaybePendingBlockWithTxHashes::PendingBlock(_) => Ok(None),
        }
    }

    /// Get the number of transactions in a block given a block hash.
    /// The number of transactions in a block.
    /// # Arguments
    /// * `hash(H256)` - The block hash.
    /// # Returns
    ///
    ///  * `transaction_count(U256)` - The number of transactions.
    ///
    /// `Ok(Option<U256>)` if the operation was successful.
    /// `Err(KakarotClientError)` if the operation failed.
    async fn block_transaction_count_by_hash(
        &self,
        hash: H256,
    ) -> Result<Option<U256>, KakarotClientError> {
        let starknet_block_id = ethers_block_id_to_starknet_block_id(BlockId::Hash(hash))?;
        let starknet_block = self
            .client
            .get_block_with_tx_hashes(&starknet_block_id)
            .await?;
        match starknet_block {
            MaybePendingBlockWithTxHashes::Block(block) => {
                Ok(Some(U256::from(block.transactions.len())))
            }
            MaybePendingBlockWithTxHashes::PendingBlock(_) => Ok(None),
        }
    }
    async fn transaction_by_block_number_and_index(
        &self,
        block_id: StarknetBlockId,
        tx_index: Index,
    ) -> Result<EtherTransaction, KakarotClientError> {
        let usize_index: usize = tx_index.into();
        let index: u64 = usize_index as u64;
        let starknet_tx = self
            .client
            .get_transaction_by_block_id_and_index(&block_id, index)
            .await?;
        let tx_hash = match &starknet_tx {
            StarknetTransaction::Invoke(InvokeTransaction::V0(tx)) => tx.transaction_hash,
            StarknetTransaction::Invoke(InvokeTransaction::V1(tx)) => tx.transaction_hash,
            StarknetTransaction::L1Handler(tx) => tx.transaction_hash,
            StarknetTransaction::Declare(tx) => tx.transaction_hash,
            StarknetTransaction::Deploy(tx) => tx.transaction_hash,
            StarknetTransaction::DeployAccount(tx) => tx.transaction_hash,
        };
        let tx_receipt = self.client.get_transaction_receipt(tx_hash).await?;
        let (blockhash_opt, blocknum_opt) = match tx_receipt {
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::Invoke(tr)) => (
                Some(PrimitiveH256::from_slice(&(tr.block_hash).to_bytes_be())),
                Some(U256::from(tr.block_number)),
            ),
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::L1Handler(tr)) => (
                Some(PrimitiveH256::from_slice(&(tr.block_hash).to_bytes_be())),
                Some(U256::from(tr.block_number)),
            ),
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::Declare(tr)) => (
                Some(PrimitiveH256::from_slice(&(tr.block_hash).to_bytes_be())),
                Some(U256::from(tr.block_number)),
            ),
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::Deploy(tr)) => (
                Some(PrimitiveH256::from_slice(&(tr.block_hash).to_bytes_be())),
                Some(U256::from(tr.block_number)),
            ),
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::DeployAccount(
                tr,
            )) => (
                Some(PrimitiveH256::from_slice(&(tr.block_hash).to_bytes_be())),
                Some(U256::from(tr.block_number)),
            ),
            MaybePendingTransactionReceipt::PendingReceipt(_) => (None, None),
        };
        let eth_tx = self
            .starknet_tx_into_eth_tx(starknet_tx, blockhash_opt, blocknum_opt)
            .await?;
        Ok(eth_tx)
    }

    /// Returns the Starknet address associated with a given Ethereum address.
    ///
    /// ## Arguments
    /// * `ethereum_address` - The Ethereum address to convert to a Starknet address.
    /// * `starknet_block_id` - The block ID to use for the Starknet contract call.
    ///
    async fn compute_starknet_address(
        &self,
        ethereum_address: Address,
        starknet_block_id: StarknetBlockId,
    ) -> Result<FieldElement, KakarotClientError> {
        let address_hex = hex::encode(ethereum_address);

        let ethereum_address_felt = FieldElement::from_hex_be(&address_hex).map_err(|e| {
            KakarotClientError::OtherError(anyhow::anyhow!(
                "Kakarot Core: Failed to convert Ethereum address to FieldElement: {}",
                e
            ))
        })?;

        let request = FunctionCall {
            contract_address: self.kakarot_main_contract,
            entry_point_selector: COMPUTE_STARKNET_ADDRESS,
            calldata: vec![ethereum_address_felt],
        };

        let starknet_contract_address = self.client.call(request, &starknet_block_id).await?;

        let result = starknet_contract_address.first().ok_or_else(|| {
            KakarotClientError::OtherError(anyhow::anyhow!(
                "Kakarot Core: Failed to get Starknet address from Kakarot"
            ))
        })?;

        println!("Starknet address: {result}");

        Ok(*result)
    }

    async fn submit_starknet_transaction(
        &self,
        max_fee: FieldElement,
        signature: Vec<FieldElement>,
        nonce: FieldElement,
        sender_address: FieldElement,
        calldata: Vec<FieldElement>,
    ) -> Result<H256, KakarotClientError> {
        let transaction_v1 = BroadcastedInvokeTransactionV1 {
            max_fee,
            signature,
            nonce,
            sender_address,
            calldata,
        };

        let transaction_result = self
            .client
            .add_invoke_transaction(&BroadcastedInvokeTransaction::V1(transaction_v1))
            .await?;

        println!(
            "Kakarot Core: Submitted Starknet transaction with hash: {0}",
            transaction_result.transaction_hash,
        );

        Ok(H256::from(
            transaction_result.transaction_hash.to_bytes_be(),
        ))
    }

    /// Returns the receipt of a transaction by transaction hash.
    ///
    /// # Arguments
    ///
    /// * `hash(H256)` - The block hash.
    ///
    /// # Returns
    ///
    ///  * `transaction_receipt(TransactionReceipt)` - The transaction receipt.
    ///
    /// `Ok(Option<TransactionReceipt>)` if the operation was successful.
    /// `Err(KakarotClientError)` if the operation failed.
    async fn get_transaction_receipt(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionReceipt>, KakarotClientError> {
        let mut res_receipt = create_default_transaction_receipt();

        //TODO: Error when trying to transform 32 bytes hash to FieldElement
        let hash_felt = hash_to_field_element(PrimitiveH256::from(hash.0))?;
        let starknet_tx_receipt = self.client.get_transaction_receipt(hash_felt).await?;

        match starknet_tx_receipt {
            MaybePendingTransactionReceipt::Receipt(receipt) => match receipt {
                StarknetTransactionReceipt::Invoke(InvokeTransactionReceipt {
                    transaction_hash,
                    status,
                    block_hash,
                    block_number,
                    ..
                })
                | StarknetTransactionReceipt::Deploy(DeployTransactionReceipt {
                    transaction_hash,
                    status,
                    block_hash,
                    block_number,
                    ..
                })
                | StarknetTransactionReceipt::DeployAccount(DeployAccountTransactionReceipt {
                    transaction_hash,
                    status,
                    block_hash,
                    block_number,
                    ..
                }) => {
                    res_receipt.transaction_hash =
                        Some(PrimitiveH256::from_slice(&transaction_hash.to_bytes_be()));
                    res_receipt.status_code = match status {
                        StarknetTransactionStatus::Pending => Some(U64::from(0)),
                        StarknetTransactionStatus::AcceptedOnL1 => Some(U64::from(1)),
                        StarknetTransactionStatus::AcceptedOnL2 => Some(U64::from(1)),
                        StarknetTransactionStatus::Rejected => Some(U64::from(0)),
                    };
                    res_receipt.block_hash =
                        Some(PrimitiveH256::from_slice(&block_hash.to_bytes_be()));
                    res_receipt.block_number = Some(felt_to_u256(block_number.into()));
                }
                // L1Handler and Declare transactions not supported for now in Kakarot
                StarknetTransactionReceipt::L1Handler(_)
                | StarknetTransactionReceipt::Declare(_) => return Ok(None),
            },
            MaybePendingTransactionReceipt::PendingReceipt(_) => {
                return Ok(None);
            }
        };

        let starknet_tx = self.client.get_transaction_by_hash(hash_felt).await?;
        match starknet_tx.clone() {
            StarknetTransaction::Invoke(invoke_tx) => {
                match invoke_tx {
                    InvokeTransaction::V0(v0) => {
                        let eth_address = self
                            .safe_get_evm_address(
                                v0.contract_address,
                                StarknetBlockId::Tag(BlockTag::Latest),
                            )
                            .await;
                        res_receipt.contract_address = Some(eth_address);
                    }
                    InvokeTransaction::V1(_) => {}
                };
            }
            StarknetTransaction::DeployAccount(_) | StarknetTransaction::Deploy(_) => {}
            _ => return Ok(None),
        };

        let eth_tx = self.starknet_tx_into_eth_tx(starknet_tx, None, None).await;
        match eth_tx {
            Ok(tx) => {
                res_receipt.from = tx.from;
                res_receipt.to = tx.to;
            }
            _ => {
                return Ok(None);
            }
        };

        Ok(Some(res_receipt))
    }

    async fn get_evm_address(
        &self,
        starknet_address: FieldElement,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Address, KakarotClientError> {
        let request = FunctionCall {
            contract_address: starknet_address,
            entry_point_selector: GET_EVM_ADDRESS,
            calldata: vec![],
        };

        let evm_address_felt = self.client.call(request, &starknet_block_id).await?;
        let evm_address = evm_address_felt
            .first()
            .ok_or_else(|| {
                KakarotClientError::OtherError(anyhow::anyhow!(
                    "Kakarot Core: Failed to get EVM address from smart contract on Kakarot"
                ))
            })?
            .to_bytes_be();

        // Workaround as .get(12..32) does not dynamically size the slice
        let slice: &[u8] = evm_address.get(12..32).ok_or_else(|| {
            KakarotClientError::OtherError(anyhow::anyhow!(
                "Kakarot Core: Failed to cast EVM address from 32 bytes to 20 bytes EVM format"
            ))
        })?;
        let mut tmp_slice = [0u8; 20];
        tmp_slice.copy_from_slice(slice);
        let evm_address_sliced = &tmp_slice;

        Ok(Address::from(evm_address_sliced))
    }

    async fn starknet_tx_into_eth_tx(
        &self,
        tx: StarknetTransaction,
        block_hash: Option<PrimitiveH256>,
        block_number: Option<U256>,
    ) -> Result<EtherTransaction, KakarotClientError> {
        let mut ether_tx = EtherTransaction::default();
        let class_hash;

        match tx {
            StarknetTransaction::Invoke(invoke_tx) => {
                match invoke_tx {
                    InvokeTransaction::V0(v0) => {
                        // Extract relevant fields from InvokeTransactionV0 and convert them to the corresponding fields in EtherTransaction
                        ether_tx.hash =
                            PrimitiveH256::from_slice(&v0.transaction_hash.to_bytes_be());
                        class_hash = self
                            .client
                            .get_class_hash_at(
                                &StarknetBlockId::Tag(BlockTag::Latest),
                                v0.contract_address,
                            )
                            .await?;

                        ether_tx.nonce = felt_to_u256(v0.nonce);
                        ether_tx.from = starknet_address_to_ethereum_address(v0.contract_address);
                        // Define gas_price data
                        ether_tx.gas_price = None;
                        // Extracting the signature
                        ether_tx.r = felt_option_to_u256(v0.signature.get(0))?;
                        ether_tx.s = felt_option_to_u256(v0.signature.get(1))?;
                        ether_tx.v = felt_option_to_u256(v0.signature.get(2))?;
                        // Extracting the data (transform from calldata)
                        ether_tx.input = vec_felt_to_bytes(v0.calldata);
                        //TODO:  Fetch transaction To
                        ether_tx.to = None;
                        //TODO:  Fetch value
                        ether_tx.value = U256::from(100);
                        //TODO: Fetch Gas
                        ether_tx.gas = U256::from(100);
                        // Extracting the chain_id
                        ether_tx.chain_id = Some(CHAIN_ID.into());
                        // Extracting the standard_v
                        ether_tx.standard_v = U256::from(0);
                        // Extracting the creates
                        ether_tx.creates = None;
                        // How to fetch the public_key?
                        ether_tx.public_key = None;
                        // ...
                        ether_tx.block_hash = block_hash;
                        ether_tx.block_number = block_number;
                    }

                    InvokeTransaction::V1(v1) => {
                        // Extract relevant fields from InvokeTransactionV0 and convert them to the corresponding fields in EtherTransaction

                        ether_tx.hash =
                            PrimitiveH256::from_slice(&v1.transaction_hash.to_bytes_be());
                        class_hash = self
                            .client
                            .get_class_hash_at(
                                &StarknetBlockId::Tag(BlockTag::Latest),
                                v1.sender_address,
                            )
                            .await?;

                        ether_tx.nonce = felt_to_u256(v1.nonce);
                        ether_tx.from = starknet_address_to_ethereum_address(v1.sender_address);
                        // Define gas_price data
                        ether_tx.gas_price = None;
                        // Extracting the signature
                        ether_tx.r = felt_option_to_u256(v1.signature.get(0))?;
                        ether_tx.s = felt_option_to_u256(v1.signature.get(1))?;
                        ether_tx.v = felt_option_to_u256(v1.signature.get(2))?;
                        // Extracting the data
                        ether_tx.input = vec_felt_to_bytes(v1.calldata);
                        ether_tx.to = None;
                        // Extracting the to address
                        // TODO: Get Data from Calldata
                        ether_tx.to = None;
                        // Extracting the value
                        ether_tx.value = U256::from(100);
                        // TODO:: Get Gas from Estimate
                        ether_tx.gas = U256::from(100);
                        // Extracting the chain_id
                        ether_tx.chain_id = Some(CHAIN_ID.into());
                        // Extracting the standard_v
                        ether_tx.standard_v = U256::from(0);
                        // Extracting the creates
                        ether_tx.creates = None;
                        // Extracting the public_key
                        ether_tx.public_key = None;
                        // Extracting the access_list
                        ether_tx.access_list = None;
                        // Extracting the transaction_type
                        ether_tx.transaction_type = None;
                        ether_tx.block_hash = block_hash;
                        ether_tx.block_number = block_number;
                    }
                }
            }
            // Repeat the process for each variant of StarknetTransaction
            StarknetTransaction::L1Handler(l1_handler_tx) => {
                // Extract relevant fields from InvokeTransactionV0 and convert them to the corresponding fields in EtherTransaction
                ether_tx.hash =
                    PrimitiveH256::from_slice(&l1_handler_tx.transaction_hash.to_bytes_be());
                class_hash = self
                    .client
                    .get_class_hash_at(
                        &StarknetBlockId::Tag(BlockTag::Latest),
                        l1_handler_tx.contract_address,
                    )
                    .await?;

                ether_tx.nonce = U256::from(l1_handler_tx.nonce);
                ether_tx.from = self
                    .get_evm_address(
                        l1_handler_tx.contract_address,
                        StarknetBlockId::Tag(BlockTag::Latest),
                    )
                    .await?;
                // Define gas_price data
                ether_tx.gas_price = None;
                // Extracting the data
                ether_tx.input = vec_felt_to_bytes(l1_handler_tx.calldata);
                // Extracting the to address
                ether_tx.to = None;
                // Extracting the value
                ether_tx.value = U256::from(0);
                // TODO: Get from estimate gas
                ether_tx.gas = U256::from(0);
                // Extracting the chain_id
                ether_tx.chain_id = Some(CHAIN_ID.into());
                // Extracting the creates
                ether_tx.creates = None;
                // Extracting the public_key
                ether_tx.public_key = None;
                ether_tx.block_hash = block_hash;
                ether_tx.block_number = block_number;
            }
            StarknetTransaction::Declare(declare_tx) => {
                // Extract relevant fields from InvokeTransactionV0 and convert them to the corresponding fields in EtherTransaction
                ether_tx.hash =
                    PrimitiveH256::from_slice(&declare_tx.transaction_hash.to_bytes_be());
                class_hash = self
                    .client
                    .get_class_hash_at(
                        &StarknetBlockId::Tag(BlockTag::Latest),
                        declare_tx.sender_address,
                    )
                    .await?;
                ether_tx.nonce = felt_to_u256(declare_tx.nonce);
                ether_tx.from = starknet_address_to_ethereum_address(declare_tx.sender_address);
                // Define gas_price data
                ether_tx.gas_price = None;
                // Extracting the signature
                ether_tx.r = felt_option_to_u256(declare_tx.signature.get(0))?;
                ether_tx.s = felt_option_to_u256(declare_tx.signature.get(1))?;
                ether_tx.v = felt_option_to_u256(declare_tx.signature.get(2))?;
                // Extracting the to address
                ether_tx.to = None;
                // Extracting the value
                ether_tx.value = U256::from(0);
                // Extracting the gas
                ether_tx.gas = U256::from(0);
                // Extracting the chain_id
                ether_tx.chain_id = Some(CHAIN_ID.into());
                // Extracting the standard_v
                ether_tx.standard_v = U256::from(0);
                // Extracting the public_key
                ether_tx.public_key = None;
                ether_tx.block_hash = block_hash;
                ether_tx.block_number = block_number;
            }
            StarknetTransaction::Deploy(deploy_tx) => {
                // Extract relevant fields from InvokeTransactionV0 and convert them to the corresponding fields in EtherTransaction
                ether_tx.hash =
                    PrimitiveH256::from_slice(&deploy_tx.transaction_hash.to_bytes_be());
                class_hash = deploy_tx.class_hash;
                // Define gas_price data
                ether_tx.gas_price = None;

                ether_tx.creates = None;
                // Extracting the public_key
                ether_tx.public_key = None;
                ether_tx.to = None;
                ether_tx.block_hash = block_hash;
                ether_tx.block_number = block_number;
            }
            StarknetTransaction::DeployAccount(deploy_account_tx) => {
                ether_tx.hash =
                    PrimitiveH256::from_slice(&deploy_account_tx.transaction_hash.to_bytes_be());
                class_hash = deploy_account_tx.class_hash;

                ether_tx.nonce = felt_to_u256(deploy_account_tx.nonce);
                // TODO: Get from estimate gas
                ether_tx.gas_price = None;
                // Extracting the signature
                ether_tx.r = felt_option_to_u256(deploy_account_tx.signature.get(0))?;
                ether_tx.s = felt_option_to_u256(deploy_account_tx.signature.get(1))?;
                ether_tx.v = felt_option_to_u256(deploy_account_tx.signature.get(2))?;
                // Extracting the to address
                ether_tx.to = None;
                // Extracting the gas
                ether_tx.gas = U256::from(0);
                // Extracting the chain_id
                ether_tx.chain_id = Some(CHAIN_ID.into());
                // Extracting the standard_v
                ether_tx.standard_v = U256::from(0);
                // Extracting the public_key
                ether_tx.public_key = None;
                ether_tx.block_hash = block_hash;
                ether_tx.block_number = block_number;
            }
        }
        let kakarot_class_hash = FieldElement::from_hex_be(KAKAROT_CONTRACT_ACCOUNT_CLASS_HASH)
            .map_err(|e| {
                KakarotClientError::OtherError(anyhow::anyhow!(
                    "Failed to convert Starknet block hash to FieldElement: {}",
                    e
                ))
            })?;
        let kakarot_starknet_address =
            FieldElement::from_hex_be(KAKAROT_CONTRACT_ACCOUNT_CLASS_HASH).map_err(|e| {
                KakarotClientError::OtherError(anyhow::anyhow!(
                    "Failed to convert Starknet block hash to FieldElement: {}",
                    e
                ))
            })?;
        if class_hash == kakarot_class_hash {
            ether_tx.to = Some(starknet_address_to_ethereum_address(
                kakarot_starknet_address,
            ));
            Ok(ether_tx)
        } else {
            Err(KakarotClientError::OtherError(anyhow::anyhow!(
                "Kakarot Filter: Tx is not part of Kakarot"
            )))
        }
    }

    async fn starknet_block_to_eth_block(
        &self,
        block: MaybePendingStarknetBlock,
    ) -> Result<RichBlock, KakarotClientError> {
        // Fixed fields in the Ethereum block as Starknet does not have these fields

        //InvokeTransactionReceipt -
        //TODO: Fetch real data
        let gas_limit = U256::from(1000000); // Hard Code
                                             //TODO: Fetch real data
        let gas_used = U256::from(500000); // Hard Code (Sum of actual_fee's)
                                           //TODO: Fetch real data
        let difficulty = U256::from(1000000); // Fixed
                                              //TODO: Fetch real data
        let nonce: Option<H64> = Some(H64::from_low_u64_be(0));
        //TODO: Fetch real data
        let size: Option<U256> = Some(U256::from(100));
        // Bloom is a byte array of length 256
        let logs_bloom = Bloom::default();
        let extra_data = Bytes::from(b"0x00");
        //TODO: Fetch real data
        let total_difficulty: U256 = U256::from(1000000);
        //TODO: Fetch real data
        let base_fee_per_gas = U256::from(32);
        //TODO: Fetch real data
        let mix_hash = PrimitiveH256::from_low_u64_be(0);

        match block {
            MaybePendingStarknetBlock::BlockWithTxHashes(maybe_pending_block) => {
                match maybe_pending_block {
                    MaybePendingBlockWithTxHashes::PendingBlock(pending_block_with_tx_hashes) => {
                        let parent_hash = PrimitiveH256::from_slice(
                            &pending_block_with_tx_hashes.parent_hash.to_bytes_be(),
                        );
                        let sequencer = H160::from_slice(
                            &pending_block_with_tx_hashes.sequencer_address.to_bytes_be()[12..32],
                        );
                        let timestamp = U256::from_be_bytes(
                            pending_block_with_tx_hashes.timestamp.to_be_bytes(),
                        );
                        //TODO: Add filter to tx_hashes
                        let transactions = BlockTransactions::Hashes(
                            pending_block_with_tx_hashes
                                .transactions
                                .into_iter()
                                .map(|tx| PrimitiveH256::from_slice(&tx.to_bytes_be()))
                                .collect(),
                        );

                        let header = Header {
                            // PendingblockWithTxHashes doesn't have a block hash
                            hash: None,
                            parent_hash,
                            uncles_hash: parent_hash,
                            author: sequencer,
                            miner: sequencer,
                            // PendingblockWithTxHashes doesn't have a state root
                            state_root: PrimitiveH256::zero(),
                            // PendingblockWithTxHashes doesn't have a transactions root
                            transactions_root: PrimitiveH256::zero(),
                            // PendingblockWithTxHashes doesn't have a receipts root
                            receipts_root: PrimitiveH256::zero(),
                            // PendingblockWithTxHashes doesn't have a block number
                            number: None,
                            gas_used,
                            gas_limit,
                            extra_data,
                            logs_bloom,
                            timestamp,
                            difficulty,
                            nonce,
                            size,
                            base_fee_per_gas,
                            mix_hash,
                        };
                        let block = Block {
                            header,
                            total_difficulty,
                            uncles: vec![],
                            transactions,
                            base_fee_per_gas: None,
                            size,
                        };
                        Ok(Rich::<Block> {
                            inner: block,
                            extra_info: BTreeMap::default(),
                        })
                    }
                    MaybePendingBlockWithTxHashes::Block(block_with_tx_hashes) => {
                        let hash = PrimitiveH256::from_slice(
                            &block_with_tx_hashes.block_hash.to_bytes_be(),
                        );
                        let parent_hash = PrimitiveH256::from_slice(
                            &block_with_tx_hashes.parent_hash.to_bytes_be(),
                        );
                        let sequencer = H160::from_slice(
                            &block_with_tx_hashes.sequencer_address.to_bytes_be()[12..32],
                        );
                        let state_root =
                            PrimitiveH256::from_slice(&block_with_tx_hashes.new_root.to_bytes_be());
                        let number = U256::from(block_with_tx_hashes.block_number);
                        let timestamp = U256::from(block_with_tx_hashes.timestamp);
                        //TODO: Add filter to tx_hashes
                        let transactions = BlockTransactions::Hashes(
                            block_with_tx_hashes
                                .transactions
                                .into_iter()
                                .map(|tx| PrimitiveH256::from_slice(&tx.to_bytes_be()))
                                .collect(),
                        );
                        let header = Header {
                            hash: Some(hash),
                            parent_hash,
                            uncles_hash: parent_hash,
                            author: sequencer,
                            miner: sequencer,
                            state_root,
                            // BlockWithTxHashes doesn't have a transactions root
                            transactions_root: PrimitiveH256::zero(),
                            // BlockWithTxHashes doesn't have a receipts root
                            receipts_root: PrimitiveH256::zero(),
                            number: Some(number),
                            gas_used,
                            gas_limit,
                            extra_data,
                            logs_bloom,
                            timestamp,
                            difficulty,
                            nonce,
                            size,
                            base_fee_per_gas,
                            mix_hash,
                        };
                        let block = Block {
                            header,
                            total_difficulty,
                            uncles: vec![],
                            transactions,
                            base_fee_per_gas: None,
                            size,
                        };
                        Ok(Rich::<Block> {
                            inner: block,
                            extra_info: BTreeMap::default(),
                        })
                    }
                }
            }
            MaybePendingStarknetBlock::BlockWithTxs(maybe_pending_block) => {
                match maybe_pending_block {
                    MaybePendingBlockWithTxs::PendingBlock(pending_block_with_txs) => {
                        let parent_hash = PrimitiveH256::from_slice(
                            &pending_block_with_txs.parent_hash.to_bytes_be(),
                        );
                        let sequencer = H160::from_slice(
                            &pending_block_with_txs.sequencer_address.to_bytes_be()[12..32],
                        );
                        let timestamp =
                            U256::from_be_bytes(pending_block_with_txs.timestamp.to_be_bytes());

                        let transactions = self
                            .filter_transactions(pending_block_with_txs.transactions, None, None)
                            .await?;
                        let header = Header {
                            // PendingBlockWithTxs doesn't have a block hash
                            hash: None,
                            parent_hash,
                            uncles_hash: parent_hash,
                            author: sequencer,
                            miner: sequencer,
                            // PendingBlockWithTxs doesn't have a state root
                            state_root: PrimitiveH256::zero(),
                            // PendingBlockWithTxs doesn't have a transactions root
                            transactions_root: PrimitiveH256::zero(),
                            // PendingBlockWithTxs doesn't have a receipts root
                            receipts_root: PrimitiveH256::zero(),
                            // PendingBlockWithTxs doesn't have a block number
                            number: None,
                            gas_used,
                            gas_limit,
                            extra_data,
                            logs_bloom,
                            timestamp,
                            difficulty,
                            nonce,
                            size,
                            base_fee_per_gas,
                            mix_hash,
                        };
                        let block = Block {
                            header,
                            total_difficulty,
                            uncles: vec![],
                            transactions,
                            base_fee_per_gas: None,
                            size,
                        };
                        Ok(Rich::<Block> {
                            inner: block,
                            extra_info: BTreeMap::default(),
                        })
                    }
                    MaybePendingBlockWithTxs::Block(block_with_txs) => {
                        let hash =
                            PrimitiveH256::from_slice(&block_with_txs.block_hash.to_bytes_be());
                        let parent_hash =
                            PrimitiveH256::from_slice(&block_with_txs.parent_hash.to_bytes_be());
                        let sequencer = H160::from_slice(
                            &block_with_txs.sequencer_address.to_bytes_be()[12..32],
                        );
                        let state_root =
                            PrimitiveH256::from_slice(&block_with_txs.new_root.to_bytes_be());
                        let transactions_root = PrimitiveH256::from_slice(
                            &"0xac91334ba861cb94cba2b1fd63df7e87c15ca73666201abd10b5462255a5c642"
                                .as_bytes()[1..33],
                        );
                        let receipts_root = PrimitiveH256::from_slice(
                            &"0xf2c8755adf35e78ffa84999e48aba628e775bb7be3c70209738d736b67a9b549"
                                .as_bytes()[1..33],
                        );

                        let number = U256::from(block_with_txs.block_number);
                        let timestamp = U256::from(block_with_txs.timestamp);

                        let blockhash_opt = Some(PrimitiveH256::from_slice(
                            &(block_with_txs.block_hash).to_bytes_be(),
                        ));
                        let blocknum_opt = Some(U256::from(block_with_txs.block_number));

                        let transactions = self
                            .filter_transactions(
                                block_with_txs.transactions,
                                blockhash_opt,
                                blocknum_opt,
                            )
                            .await?;

                        let header = Header {
                            hash: Some(hash),
                            parent_hash,
                            uncles_hash: parent_hash,
                            author: sequencer,
                            miner: sequencer,
                            state_root,
                            // BlockWithTxHashes doesn't have a transactions root
                            transactions_root,
                            // BlockWithTxHashes doesn't have a receipts root
                            receipts_root,
                            number: Some(number),
                            gas_used,
                            gas_limit,
                            extra_data,
                            logs_bloom,
                            timestamp,
                            difficulty,
                            nonce,
                            size,
                            base_fee_per_gas,
                            mix_hash,
                        };
                        let block = Block {
                            header,
                            total_difficulty,
                            uncles: vec![],
                            transactions,
                            base_fee_per_gas: None,
                            size,
                        };
                        Ok(Rich::<Block> {
                            inner: block,
                            extra_info: BTreeMap::default(),
                        })
                    }
                }
            }
        }
    }
    async fn filter_transactions(
        &self,
        initial_transactions: Vec<StarknetTransaction>,
        blockhash_opt: Option<PrimitiveH256>,
        blocknum_opt: Option<U256>,
    ) -> Result<BlockTransactions, KakarotClientError> {
        let mut transactions_vec = vec![];
        for transaction in initial_transactions {
            let tx_value = self
                .starknet_tx_into_eth_tx(transaction, blockhash_opt, blocknum_opt)
                .await;
            if let Ok(val) = tx_value {
                transactions_vec.push(val)
            }
        }
        Ok(BlockTransactions::Full(transactions_vec))
    }
}
