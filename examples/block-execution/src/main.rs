use reth::{
    api::NodeTypesWithDBAdapter,
    beacon_consensus::EthBeaconConsensus,
    providers::{
        providers::{BlockchainProvider, StaticFileProvider},
        ProviderFactory,
    },
    rpc::eth::EthApi,
    utils::open_db_read_only,
};

use reth_node_ethereum::{
    node::EthereumEngineValidator, EthEvmConfig, EthExecutorProvider, EthereumNode,
};

use reth::rpc::builder::{
    RethRpcModule, RpcModuleBuilder, RpcServerConfig, TransportRpcModuleConfig, RpcServerHandle,
};

use reth::tasks::TokioTaskExecutor;

use alloy_primitives::{Address, B256, Bloom, Bytes, FixedBytes, U256, TxKind};
use alloy_rpc_types_engine::ExecutionPayloadV1;
use eyre::Result;
use reth_primitives::{Block, BlockBody, Header, RecoveredBlock, Transaction, TransactionSigned};
use alloy_consensus::{TxEip1559, BlockHeader, SignableTransaction};
use reth_provider::{
    BlockWriter, AccountReader, StateProviderFactory, DatabaseProviderFactory,
};
use reth_revm::database::StateProviderDatabase;
use reth_chainspec::{ChainSpecBuilder, ChainSpec};
use reth_evm::execute::{BlockExecutorProvider, Executor, ExecutionOutcome};
use reth_trie::{HashedPostStateSorted, updates::TrieUpdates};
use std::{sync::Arc, str::FromStr, collections::BTreeMap};
use serde_json;
use alloy_genesis::{Genesis, ChainConfig, GenesisAccount};
use reth_db_common::init::init_genesis;
use alloy_signer_local::{coins_bip39::English, MnemonicBuilder, LocalSigner};
use alloy_signer::Signer;
use k256::ecdsa::SigningKey;
use tokio::runtime::Runtime;
use reth_db::{mdbx::DatabaseArguments, DatabaseEnv};
use std::path::PathBuf;
use std::io::{BufWriter, BufReader, Write, Read};
use std::fs::File;
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use reth_primitives_traits::transaction::signed::SignedTransaction;
use std::time::Instant;
// use reth_node_core::{
//     consensus::beacon::BeaconConsensus as EthBeaconConsensus,
//     eth::EthEvmConfig,
//     rpc::{
//         eth::EthApi,
//         builder::{RethRpcModule, RpcModuleBuilder, RpcServerConfig, TransportRpcModuleConfig},
//     },
//     task::TokioTaskExecutor,
// };
use futures::future;
use reqwest::Client;
use serde_json::{json, Value};

/// Whether to read genesis configuration from disk
const READ_GENESIS_FROM_DISK: bool = false;

/// Path to the genesis file
const GENESIS_FILE: &str = "./data/genesis.json";

/// Read genesis configuration from a JSON file
fn read_genesis_from_file(path: &str) -> Result<Genesis> {
    let genesis_json = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&genesis_json)?)
}

/// Create a default genesis configuration
fn create_default_genesis(sender: Address) -> Genesis {
    // Create genesis configuration with pre-funded accounts
    let mut alloc = BTreeMap::new();
    alloc.insert(
        sender,
        GenesisAccount {
            balance: U256::from_str("1000000000000000000000").unwrap(), // 1000 ETH
            ..Default::default()
        },
    );
    
    // Create genesis configuration
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

/// Generate test blocks that transfer ETH from sender to recipient and write them to a file
fn generate_test_blocks(
    signer: &LocalSigner<SigningKey>, 
    recipient: Address, 
    txs_per_block: usize, 
    num_blocks: usize,
    output_file: &str,
) -> Result<()> {
    println!("Generating {} blocks with {} transactions each...", num_blocks, txs_per_block);
    
    // Create runtime for async transaction creation
    let rt = Runtime::new()?;
    let nonce_counter = AtomicU64::new(0);
    
    // Open file for writing
    let file = File::create(output_file)?;
    let mut writer = BufWriter::new(file);
    
    for block_num in 0..num_blocks {
        // Generate transactions in parallel
        let mut block_txs: Vec<TransactionSigned> = (0..txs_per_block)
            .into_par_iter()
            .map(|_| {
                let nonce = nonce_counter.fetch_add(1, Ordering::SeqCst);
                rt.block_on(create_test_transaction(
                    signer,
                    recipient,
                    U256::from(10_000_000_000_000_000u64), // 0.01 ETH
                    nonce,
                )).expect("Failed to create transaction")
            })
            .collect();

        // Sort transactions by nonce
        block_txs.sort_by_key(|tx| {
            if let Transaction::Eip1559(ref t) = tx.transaction() {
                t.nonce
            } else {
                unreachable!("We only create EIP1559 transactions")
            }
        });
        
        // Create block and convert to serialized form
        let block = create_test_block(block_txs);
        let payload = ExecutionPayloadV1::from_block_slow(&block);
        let bytes = serde_json::to_vec(&payload)?;
        
        // Write length of serialized block followed by the block itself
        writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
        writer.write_all(&bytes)?;
        writer.flush()?;
        
        println!("Generated and wrote block {} with {} transactions (size: {:.2} MB)", 
            block_num + 1, 
            txs_per_block,
            bytes.len() as f64 / 1_000_000.0
        );
    }

    Ok(())
}

/// Tracks and reports timing information for block execution steps
struct BlockExecutionTimer {
    start_time: Instant,
    starts: BTreeMap<&'static str, Instant>,
    timings: BTreeMap<&'static str, f64>,
}

impl BlockExecutionTimer {
    fn new() -> Self {
        Self {
            start_time: Instant::now(),
            starts: BTreeMap::new(),
            timings: BTreeMap::new(),
        }
    }

    fn start(&mut self, name: &'static str) {
        self.starts.insert(name, Instant::now());
    }

    fn end(&mut self, name: &'static str) {
        if let Some(start) = self.starts.remove(name) {
            self.timings.insert(name, start.elapsed().as_secs_f64());
        }
    }

    fn report(&self, block_number: u64, num_txs: usize, gas_used: u64, num_receipts: usize) {
        let total_time = self.start_time.elapsed().as_secs_f64();
        let tracked_time: f64 = self.timings.values().sum();
        
        println!("Block execution completed:");
        println!("  Gas used: {}", gas_used);
        println!("  Number of receipts: {}", num_receipts);
        
        println!("\nBlock {} timing breakdown:", block_number);
        for (name, duration) in &self.timings {
            println!("  {} time: {:.2}s", name, duration);
        }
        println!("  Total time: {:.2}s", total_time);
        println!("  Other overhead: {:.2}s", total_time - tracked_time);
        println!("  Transactions per second: {:.2}", num_txs as f64 / total_time);
    }
}

/// Handles block execution and database interactions
#[derive(Clone)]
struct BlockExecutor {
    blockchain: Arc<BlockchainProvider<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>>,
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

        // Create blockchain provider
        let blockchain = Arc::new(BlockchainProvider::new(factory)?);

        Ok(Self { blockchain, spec })
    }

    /// Execute and commit the next block
    fn next_block(&self, block: &Block) -> Result<()> {
        println!("\nExecuting block {}...", block.number);
        if let Some(tx) = block.body.transactions.first() {
            println!("Transaction signer: {}", tx.recover_signer().unwrap());
        }

        let mut timer = BlockExecutionTimer::new();
        
        // Provider setup
        timer.start("Provider setup");
        let executor_provider = EthExecutorProvider::ethereum(self.spec.clone());
        let state_provider = self.blockchain.latest()?;
        let executor = executor_provider.executor(StateProviderDatabase::new(&state_provider));
        timer.end("Provider setup");
        
        // Block recovery
        timer.start("Block recovery");
        let recovered_block = RecoveredBlock::try_recover(block.clone())?;
        timer.end("Block recovery");
        
        // Block execution
        timer.start("Execution");
        let result = executor.execute(&recovered_block)?;
        timer.end("Execution");
        
        // Store these before moving result
        let gas_used = result.gas_used;
        let num_receipts = result.receipts.len();
        
        // Write setup
        timer.start("Write setup");
        let provider_rw = self.blockchain.database_provider_rw()?;
        let execution_outcome = ExecutionOutcome::from((result, recovered_block.number()));
        timer.end("Write setup");
        
        // State commit
        timer.start("State commit");
        provider_rw.append_blocks_with_state(
            vec![recovered_block],
            &execution_outcome,
            HashedPostStateSorted::default(),
            TrieUpdates::default(),
        )?;
        provider_rw.commit()?;
        timer.end("State commit");
        
        timer.report(block.number, block.body.transactions.len(), gas_used, num_receipts);
        
        Ok(())
    }

    /// Get account balance
    fn get_balance(&self, address: &Address) -> Result<Option<U256>> {
        let state_provider = self.blockchain.latest()?;
        Ok(state_provider.basic_account(address)?.map(|account| account.balance))
    }

    /// Start the RPC server
    pub async fn start_server(&self) -> Result<RpcServerHandle> {
        // Configure which RPC namespaces to expose
        let module_config = TransportRpcModuleConfig::default().with_http([RethRpcModule::Eth]);

        // Create the RPC module builder with our components
        let rpc_builder = RpcModuleBuilder::default()
            .with_provider((*self.blockchain).clone())
            .with_noop_pool()  // We don't need transaction pool for this example
            .with_noop_network()  // We don't need network for this example
            .with_executor(TokioTaskExecutor::default())
            .with_evm_config(EthEvmConfig::new(self.spec.clone()))
            .with_block_executor(EthExecutorProvider::ethereum(self.spec.clone()))
            .with_consensus(EthBeaconConsensus::new(self.spec.clone()));

        // Build the server modules
        let server = rpc_builder.build(
            module_config,
            Box::new(EthApi::with_spawner),
            Arc::new(EthereumEngineValidator::new(self.spec.clone())),
        );

        // Configure and start the server
        let server_config = RpcServerConfig::http(Default::default())
            .with_http_address("127.0.0.1:8545".parse().unwrap());

        let handle = server_config.start(&server).await?;

        println!("RPC server started at http://{}", handle.http_local_addr().unwrap());

        Ok(handle)
    }
}

/// Read blocks from a file one at a time
struct BlockReader {
    reader: BufReader<File>,
}

impl BlockReader {
    fn new(path: &str) -> Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            reader: BufReader::new(file),
        })
    }

    fn next_block(&mut self) -> Result<Option<Block>> {
        let mut len_bytes = [0u8; 4];
        match self.reader.read_exact(&mut len_bytes) {
            Ok(_) => {
                let len = u32::from_be_bytes(len_bytes) as usize;
                let mut block_bytes = vec![0u8; len];
                self.reader.read_exact(&mut block_bytes)?;
                
                let payload: ExecutionPayloadV1 = serde_json::from_slice(&block_bytes)?;
                Ok(Some(payload.try_into_block()?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Tests various RPC methods and validates their responses
async fn test_rpc_server(sender: Address, recipient: Address) -> Result<()> {
    let client = Client::new();
    let url = "http://127.0.0.1:8545";

    println!("\nTesting RPC methods...");

    // Helper function for making RPC calls
    async fn rpc_call(client: &Client, url: &str, method: &str, params: Value) -> Result<Value> {
        let response = client
            .post(url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params
            }))
            .send()
            .await?
            .json::<Value>()
            .await?;

        println!("\nMethod: {}", method);
        println!("Response: {}", serde_json::to_string_pretty(&response)?);

        if let Some(error) = response.get("error") {
            return Err(eyre::eyre!("RPC error: {}", error));
        }

        Ok(response.get("result").unwrap_or(&Value::Null).clone())
    }

    println!("\n=== Testing Basic Node State ===");
    
    // Get latest block number
    let block_number = rpc_call(&client, url, "eth_blockNumber", json!([])).await?;
    let latest_block_hex = block_number.as_str().unwrap();
    assert!(latest_block_hex.starts_with("0x"), "Block number should be hex");
    let latest_block_num = u64::from_str_radix(&latest_block_hex[2..], 16).unwrap();
    
    println!("\n=== Testing Block Explorer Functionality ===");
    println!("Simulating block explorer page load...");

    // Get latest 5 blocks (simulating pagination)
    for block_num in (0..=latest_block_num.min(4)).rev() {
        println!("\nFetching block {}", block_num);
        
        // Get block with full transaction objects
        let block = rpc_call(
            &client,
            url,
            "eth_getBlockByNumber",
            json!([format!("0x{:x}", block_num), true])
        ).await?;
        
        // Extract and display block info
        let block_obj = block.as_object().unwrap();
        println!("Block number: {}", block_num);
        println!("Timestamp: {}", block_obj.get("timestamp").unwrap());
        println!("Transaction count: {}", 
            block_obj.get("transactions")
                .and_then(|t| t.as_array())
                .map(|t| t.len())
                .unwrap_or(0)
        );

        // For each transaction in the block, get its receipt
        if let Some(txs) = block_obj.get("transactions").and_then(|t| t.as_array()) {
            for (i, tx) in txs.iter().take(3).enumerate() { // Only show first 3 for brevity
                let tx_hash = tx.get("hash").unwrap().as_str().unwrap();
                println!("\nTransaction {}: {}", i + 1, tx_hash);
                
                // Get transaction receipt for status and gas used
                let receipt = rpc_call(
                    &client,
                    url,
                    "eth_getTransactionReceipt",
                    json!([tx_hash])
                ).await?;
                
                if let Some(receipt_obj) = receipt.as_object() {
                    println!("Status: {}", receipt_obj.get("status").unwrap());
                    println!("Gas Used: {}", receipt_obj.get("gasUsed").unwrap());
                }
            }
        }
    }

    // Original balance and nonce checks
    let sender_balance = rpc_call(
        &client,
        url,
        "eth_getBalance",
        json!([format!("{:#x}", sender), "latest"])
    ).await?;
    assert!(sender_balance.as_str().unwrap().starts_with("0x"), "Balance should be hex");

    let recipient_balance = rpc_call(
        &client,
        url,
        "eth_getBalance",
        json!([format!("{:#x}", recipient), "latest"])
    ).await?;
    assert!(recipient_balance.as_str().unwrap().starts_with("0x"), "Balance should be hex");

    let nonce = rpc_call(
        &client,
        url,
        "eth_getTransactionCount",
        json!([format!("{:#x}", sender), "latest"])
    ).await?;
    assert!(nonce.as_str().unwrap().starts_with("0x"), "Nonce should be hex");

    println!("\nAll RPC tests completed successfully!");
    Ok(())
}

/// A simple example showing how to:
/// 1. Create a serialized block with a real transaction
/// 2. Convert it to an execution payload
/// 3. Execute it using Reth's EVM
/// 4. Store the results in the database
#[tokio::main]
async fn main() -> Result<()> {
    // Delete existing database folder if it exists
    let _ = std::fs::remove_dir_all("./data");
    
    // Create paths for database and blocks file
    let db_path = PathBuf::from("./data/db");
    let blocks_file = "./data/blocks.dat";

    // Create a wallet from mnemonic
    let signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()
        .expect("Failed to create wallet");
    
    // Get the sender address from the wallet
    let sender = signer.address();
    let recipient = Address::from_str("0x1000000000000000000000000000000000000000")?;
    
    println!("Using sender address: {}", sender);
    
    // Get genesis configuration
    let genesis = if READ_GENESIS_FROM_DISK {
        read_genesis_from_file(GENESIS_FILE)?
    } else {
        create_default_genesis(sender)
    };
    
    // Create block executor
    let executor = BlockExecutor::new(db_path, genesis)?;
    
    // Run blocking operations in a separate thread
    let executor_clone = executor.clone();
    let handle = tokio::task::spawn_blocking(move || {
        // Generate blocks with 42000 transactions each to get ~10MB blocks
        generate_test_blocks(&signer, recipient, 42000, 2, blocks_file)?;

        // Print initial balances
        if let Some(balance) = executor_clone.get_balance(&sender)? {
            println!("Initial sender balance: {}", balance);
        }
        if let Some(balance) = executor_clone.get_balance(&recipient)? {
            println!("Initial recipient balance: {}", balance);
        }

        // Read and execute blocks from file
        let mut block_reader = BlockReader::new(blocks_file)?;
        let mut block_count = 0;
        
        while let Some(block) = block_reader.next_block()? {
            executor_clone.next_block(&block)?;
            block_count += 1;

            // Print balances after each block
            if let Some(balance) = executor_clone.get_balance(&sender)? {
                println!("Sender balance: {}", balance);
            }
            if let Some(balance) = executor_clone.get_balance(&recipient)? {
                println!("Recipient balance: {}", balance);
            }
        }
        
        println!("Executed {} blocks from file", block_count);
        Ok::<(), eyre::Error>(())
    });

    // Wait for blocking operations to complete
    handle.await??;
    
    // Start the RPC server
    println!("Starting RPC server...");
    let executor_clone = executor.clone();
    let server_handle = executor_clone.start_server().await?;

    // Give the server time to start and verify it's running
    println!("Waiting for RPC server to start...");
    for _ in 0..5 {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        if let Ok(response) = reqwest::get("http://127.0.0.1:8545").await {
            if response.status() == 400 { // JSON-RPC endpoint returns 400 for GET requests
                println!("RPC server is running!");
                break;
            }
        }
    }

    // Run RPC tests
    test_rpc_server(sender, recipient).await?;

    // Clean exit
    println!("Tests completed, shutting down...");
    drop(server_handle); // Explicitly drop the server handle to shut it down
    std::process::exit(0);
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
        gas_limit: 1_000_000_000, // 1 billion gas limit to handle large blocks
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