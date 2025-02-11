use alloy_primitives::{Address, B256, Bloom, Bytes, FixedBytes, U256, TxKind};
use alloy_rpc_types_engine::ExecutionPayloadV1;
use eyre::Result;
use reth_primitives::{Block, BlockBody, Header, RecoveredBlock, Transaction, TransactionSigned};
use alloy_consensus::{TxEip1559, BlockHeader, SignableTransaction};
use reth_provider::{
    providers::StaticFileProvider,
    BlockWriter, AccountReader,
};
use reth_revm::database::StateProviderDatabase;
use reth_chainspec::{ChainSpecBuilder, ChainSpec};
use reth_node_ethereum::{EthereumNode, EthExecutorProvider};
use reth_evm::execute::{BlockExecutorProvider, Executor, ExecutionOutcome};
use reth_db::test_utils::{create_test_rw_db, create_test_static_files_dir};
use reth_trie::{HashedPostStateSorted, updates::TrieUpdates};
use std::{sync::Arc, str::FromStr, collections::BTreeMap};
use reth_primitives_traits::SignedTransaction;
use serde_json;
use alloy_genesis::{Genesis, ChainConfig, GenesisAccount};
use reth_db_common::init::init_genesis;
use alloy_signer_local::{coins_bip39::English, MnemonicBuilder, LocalSigner};
use alloy_signer::Signer;
use k256::ecdsa::SigningKey;
use tokio::runtime::Runtime;
use reth_db::{mdbx::DatabaseArguments, DatabaseEnv};
use std::path::PathBuf;
use reth_provider::ProviderFactory;
use reth_node_api::NodeTypesWithDBAdapter;

/// Test mnemonic for wallet generation
const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";

/// A custom struct to handle raw block bytes
pub struct SerializedBlock {
    bytes: Vec<u8>,
}

impl SerializedBlock {
    /// Create a new serialized block with a test transaction
    pub fn new(signer: &LocalSigner<SigningKey>, recipient: Address, value: U256, nonce: u64) -> Result<Self> {
        let rt = Runtime::new()?;
        let transaction = rt.block_on(create_test_transaction(signer, recipient, value, nonce))?;
        let block = create_test_block(vec![transaction]);
        
        // Convert block to payload
        let payload = ExecutionPayloadV1::from_block_slow(&block);
        
        // Convert payload to JSON bytes
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        Ok(Self { bytes })
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
async fn create_test_transaction(signer: &LocalSigner<SigningKey>, to: Address, value: U256, nonce: u64) -> Result<TransactionSigned> {
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
    
    // Sign the transaction with our private key
    let signature_hash = tx.signature_hash();
    let signature = signer.sign_hash(&signature_hash).await?;
    Ok(TransactionSigned::new_unhashed(tx, signature))
}

/// Generate test blocks that transfer ETH from sender to recipient
fn generate_test_blocks(signer: &LocalSigner<SigningKey>, recipient: Address) -> Result<Vec<Block>> {
    println!("Generating blocks...");
    let first_block = SerializedBlock::new(
        signer,
        recipient,
        U256::from(1_000_000_000_000_000_000u64), // 1 ETH
        0, // nonce
    )?.into_block()?;
    
    let second_block = SerializedBlock::new(
        signer,
        recipient,
        U256::from(500_000_000_000_000_000u64), // 0.5 ETH
        1, // nonce
    )?.into_block()?;

    Ok(vec![first_block, second_block])
}

/// Create a genesis configuration with pre-funded accounts
fn create_genesis(funded_address: Address) -> Genesis {
    // Create genesis configuration with pre-funded accounts
    let mut alloc = BTreeMap::new();
    alloc.insert(
        funded_address,
        GenesisAccount {
            balance: U256::from(10_000_000_000_000_000_000u64), // 10 ETH
            ..Default::default()
        },
    );
    
    Genesis {
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
    }
}

/// Handles block execution and database interactions
struct BlockExecutor {
    factory: ProviderFactory<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
    spec: Arc<ChainSpec>,
}

impl BlockExecutor {
    /// Create a new BlockExecutor with the given database path and genesis configuration
    fn new(db_path: PathBuf, genesis: Genesis) -> Result<Self> {
        // Create static files path
        let static_files_path = db_path.join("static_files");

        // Create directories if they don't exist
        std::fs::create_dir_all(&db_path)?;
        std::fs::create_dir_all(&static_files_path)?;

        // Create chain specification
        let spec = Arc::new(
            ChainSpecBuilder::mainnet()
                .genesis(genesis)
                .build()
        );

        // Create the provider factory
        let factory = ProviderFactory::<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>::new_with_database_path(
            &db_path,
            spec.clone(),
            DatabaseArguments::default(),
            StaticFileProvider::read_write(static_files_path)?,
        )?;

        // Initialize genesis state
        init_genesis(&factory)?;

        Ok(Self { factory, spec })
    }

    /// Execute and commit the next block
    fn next_block(&self, block: &Block) -> Result<()> {
        println!("\nExecuting block {}...", block.number);
        if let Some(tx) = block.body.transactions.first() {
            println!("Transaction signer: {}", tx.recover_signer().unwrap());
        }

        let executor_provider = EthExecutorProvider::ethereum(self.spec.clone());
        let state_provider = self.factory.latest()?;
        
        let executor = executor_provider.executor(StateProviderDatabase::new(&state_provider));
        let recovered_block = RecoveredBlock::try_recover(block.clone())?;
        let result = executor.execute(&recovered_block)?;
        println!("Block execution completed:");
        println!("  Gas used: {}", result.gas_used);
        println!("  Number of receipts: {}", result.receipts.len());

        let provider_rw = self.factory.provider_rw()?;
        let execution_outcome = ExecutionOutcome::from((result, recovered_block.number()));
        provider_rw.append_blocks_with_state(
            vec![recovered_block],
            &execution_outcome,
            HashedPostStateSorted::default(),
            TrieUpdates::default(),
        )?;
        provider_rw.commit()?;
        println!("Block {} executed and stored successfully!", block.number);
        Ok(())
    }

    /// Get account balance
    fn get_balance(&self, address: &Address) -> Result<Option<U256>> {
        let state_provider = self.factory.latest()?;
        Ok(state_provider.basic_account(address)?.map(|account| account.balance))
    }
}

/// A simple example showing how to:
/// 1. Create a serialized block with a real transaction
/// 2. Convert it to an execution payload
/// 3. Execute it using Reth's EVM
/// 4. Store the results in the database
fn main() -> Result<()> {
    // Delete existing database folder if it exists
    let _ = std::fs::remove_dir_all("./data");
    
    // Create paths for database
    let db_path = PathBuf::from("./data/db");

    // Create a wallet from mnemonic
    let signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()
        .expect("Failed to create wallet");
    
    // Get the sender address from the wallet
    let sender = signer.address();
    let recipient = Address::from_str("0x1000000000000000000000000000000000000000")?;
    
    println!("Using sender address: {}", sender);
    
    // Create genesis configuration and block executor
    let genesis = create_genesis(sender);
    let executor = BlockExecutor::new(db_path, genesis)?;
    
    // Generate blocks
    let blocks = generate_test_blocks(&signer, recipient)?;

    // Print initial balances
    if let Some(balance) = executor.get_balance(&sender)? {
        println!("Initial sender balance: {}", balance);
    }
    if let Some(balance) = executor.get_balance(&recipient)? {
        println!("Initial recipient balance: {}", balance);
    }

    // Execute all blocks
    for block in blocks.iter() {
        executor.next_block(block)?;

        // Print balances after each block
        if let Some(balance) = executor.get_balance(&sender)? {
            println!("Sender balance: {}", balance);
        }
        if let Some(balance) = executor.get_balance(&recipient)? {
            println!("Recipient balance: {}", balance);
        }
    }

    Ok(())
}

/// Creates a test block with the given transactions
fn create_test_block(transactions: Vec<TransactionSigned>) -> Block {
    // Create a header with minimal data
    static NEXT_BLOCK_NUMBER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let block_number = NEXT_BLOCK_NUMBER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    
    let header = Header {
        parent_hash: B256::default(),
        ommers_hash: B256::default(),
        beneficiary: Address::default(),
        state_root: B256::default(),
        transactions_root: B256::default(),
        receipts_root: B256::default(),
        logs_bloom: Bloom::default(),
        difficulty: U256::ZERO,
        number: block_number,
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
        // Create a wallet from mnemonic
        let signer = MnemonicBuilder::<English>::default()
            .phrase(TEST_MNEMONIC)
            .build()
            .expect("Failed to create wallet");
        
        let recipient = Address::from_str("0x1000000000000000000000000000000000000000")?;
        
        let serialized_block = SerializedBlock::new(
            &signer,
            recipient,
            U256::from(1_000_000_000_000_000_000u64),
            0,
        )?;
        
        let block = serialized_block.into_block()?;
        assert_eq!(block.number, 1);
        assert_eq!(block.body.transactions.len(), 1);
        
        Ok(())
    }
}