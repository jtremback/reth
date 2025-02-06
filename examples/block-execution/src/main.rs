use alloy_primitives::{Address, B256, Bloom, Bytes, FixedBytes, U256, TxKind};
use alloy_rpc_types_engine::ExecutionPayloadV1;
use eyre::Result;
use reth_primitives::{Block, BlockBody, Header, RecoveredBlock, Transaction, TransactionSigned};
use alloy_consensus::{TxEip1559, BlockHeader};
use reth_provider::{
    providers::StaticFileProvider,
    BlockWriter, AccountReader,
};
use reth_revm::database::StateProviderDatabase;
use reth_chainspec::ChainSpecBuilder;
use reth_node_ethereum::{EthereumNode, EthExecutorProvider};
use reth_evm::execute::{BlockExecutorProvider, Executor, ExecutionOutcome};
use reth_db::test_utils::{create_test_rw_db, create_test_static_files_dir};
use reth_trie::{HashedPostStateSorted, updates::TrieUpdates};
use std::{sync::Arc, str::FromStr, collections::BTreeMap};
use reth_primitives_traits::transaction::signature::Signature;
use reth_primitives_traits::SignedTransaction;
use serde_json;
use alloy_genesis::{Genesis, ChainConfig, GenesisAccount};
use reth_db_common::init::init_genesis;

/// A custom struct to handle raw block bytes
pub struct SerializedBlock {
    bytes: Vec<u8>,
}

impl SerializedBlock {
    /// Create a new serialized block with a test transaction
    pub fn new(sender: Address, recipient: Address, value: U256, nonce: u64) -> Self {
        let transaction = create_test_transaction(sender, recipient, value, nonce);
        let block = create_test_block(vec![transaction]);
        
        // Convert block to payload
        let payload = ExecutionPayloadV1::from_block_slow(&block);
        
        // Convert payload to JSON bytes
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        Self { bytes }
    }

    /// Parse the bytes into an ExecutionPayloadV1
    pub fn into_payload(&self) -> Result<ExecutionPayloadV1, serde_json::Error> {
        serde_json::from_slice(&self.bytes)
    }

    /// Convert the payload back into a Block
    pub fn into_block(&self) -> Result<Block> {
        let payload = self.into_payload()?;
        Ok(payload.try_into_block()?)
    }
}

/// Helper function to create a signed transaction (ETH transfer)
fn create_test_transaction(_from: Address, to: Address, value: U256, nonce: u64) -> TransactionSigned {
    let tx = Transaction::Eip1559(TxEip1559 {
        chain_id: 1, // mainnet
        nonce,
        max_priority_fee_per_gas: 1_000_000_000, // 1 gwei
        max_fee_per_gas: 2_000_000_000, // 2 gwei
        gas_limit: 21_000, // standard ETH transfer
        to: TxKind::Call(to),
        value,
        input: Bytes::default(),
        access_list: Default::default(),
    });
    
    // Note: In a real scenario, you would sign this with a private key
    // For this example, we use a test signature
    TransactionSigned::new_unhashed(tx, Signature::test_signature())
}

/// A simple example showing how to:
/// 1. Create a serialized block with a real transaction
/// 2. Convert it to an execution payload
/// 3. Execute it using Reth's EVM
/// 4. Store the results in the database
fn main() -> Result<()> {
    // Set up sender and recipient addresses
    let sender = Address::from_str("0x2ec9c1f8249343B2B6D01775CC13d990fCD9c7d8")?;
    let recipient = Address::from_str("0x1000000000000000000000000000000000000000")?;
    
    // Create genesis configuration with pre-funded accounts
    let mut alloc = BTreeMap::new();
    
    // Add sender with initial balance of 10 ETH
    alloc.insert(
        sender,
        GenesisAccount {
            balance: U256::from(10_000_000_000_000_000_000u64), // 10 ETH
            ..Default::default()
        },
    );
    
    // Create genesis configuration
    let genesis = Genesis {
        config: ChainConfig {
            chain_id: 1,
            homestead_block: Some(0),
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            berlin_block: Some(0),
            london_block: Some(0),
            shanghai_time: Some(0),
            terminal_total_difficulty: Some(U256::ZERO),
            terminal_total_difficulty_passed: true,
            ..Default::default()
        },
        alloc,
        ..Default::default()
    };
    
    // Create chain specification with our genesis config
    let spec = Arc::new(
        ChainSpecBuilder::mainnet()  // Use mainnet as base configuration
            .genesis(genesis)         // Override with our custom genesis
            .build()
    );
    
    // Create a temporary database and static files directory
    let (_static_dir, static_dir_path) = create_test_static_files_dir();

    // Create the provider factory using the builder pattern
    let factory = EthereumNode::provider_factory_builder()
        .db(create_test_rw_db())
        .chainspec(spec.clone())
        .static_file(StaticFileProvider::read_write(static_dir_path)?)
        .build_provider_factory();
    
    // Initialize genesis state
    init_genesis(&factory)?;
    
    // Create a serialized block with a transaction to transfer 1 ETH
    let serialized_block = SerializedBlock::new(
        sender,
        recipient,
        U256::from(1_000_000_000_000_000_000u64), // 1 ETH
        0, // nonce
    );
    
    // Convert serialized block back to Block type
    let block = serialized_block.into_block()?;
    
    // Debug: Print recovered signer from the first transaction
    if let Some(tx) = block.body.transactions.first() {
        println!("Transaction signer: {}", tx.recover_signer().unwrap());
    }
    
    // Create block executor
    let executor = EthExecutorProvider::ethereum(spec.clone());
    
    // Debug: Check if account state was properly set up
    let state_provider = factory.latest()?;
    if let Some(account) = state_provider.basic_account(&sender)? {
        println!("Sender account found with balance: {}", account.balance);
    } else {
        println!("Warning: Sender account not found in state!");
    }
    
    // Use the state provider for execution
    let executor = executor.executor(StateProviderDatabase::new(&state_provider));

    // Execute the entire block
    let recovered_block = RecoveredBlock::try_recover(block)?;
    let result = executor.execute(&recovered_block)?;
    println!("Block execution completed:");
    println!("  Gas used: {}", result.gas_used);
    println!("  Number of receipts: {}", result.receipts.len());
    
    // Store results in a new transaction
    let provider_rw = factory.provider_rw()?;
    let execution_outcome = ExecutionOutcome::from((result, recovered_block.number()));
    provider_rw.append_blocks_with_state(
        vec![recovered_block],
        &execution_outcome,
        HashedPostStateSorted::default(),
        TrieUpdates::default(),
    )?;
    provider_rw.commit()?;

    println!("Block executed and stored successfully!");
    Ok(())
}

/// Creates a test block with the given transactions
fn create_test_block(transactions: Vec<TransactionSigned>) -> Block {
    // Create a header with minimal data
    let header = Header {
        parent_hash: B256::default(),
        ommers_hash: B256::default(),
        beneficiary: Address::default(),
        state_root: B256::default(),
        transactions_root: B256::default(),
        receipts_root: B256::default(),
        logs_bloom: Bloom::default(),
        difficulty: U256::ZERO,
        number: 1,
        gas_limit: 30_000_000,
        gas_used: 0,
        timestamp: 1234567890u64,
        extra_data: Bytes::default(),
        mix_hash: B256::default(),
        nonce: FixedBytes::new([0; 8]),
        base_fee_per_gas: Some(1_000_000_000), // 1 gwei
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
    };

    // Create a block with the transactions
    Block::new(
        header,
        BlockBody {
            transactions,
            ommers: vec![],
            withdrawals: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_serialization() -> Result<()> {
        let sender = Address::from_str("0x2ec9c1f8249343B2B6D01775CC13d990fCD9c7d8")?;
        let recipient = Address::from_str("0x1000000000000000000000000000000000000000")?;
        
        let serialized_block = SerializedBlock::new(
            sender,
            recipient,
            U256::from(1_000_000_000_000_000_000u64),
            0,
        );
        
        let block = serialized_block.into_block()?;
        assert_eq!(block.number, 1);
        assert_eq!(block.body.transactions.len(), 1);
        
        Ok(())
    }
} 