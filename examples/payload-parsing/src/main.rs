use alloy_primitives::{Address, B256, Bloom, Bytes, FixedBytes, U256};
use alloy_rpc_types_engine::ExecutionPayloadV1;
use reth_primitives::{Block, BlockBody, Header, TransactionSigned};
use serde_json;

/// A custom struct to handle raw block bytes
pub struct CustomBlockBytes {
    bytes: Vec<u8>,
}

impl CustomBlockBytes {
    /// Create a new block with some example data
    pub fn new() -> Self {
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
            timestamp: 1234567890,
            extra_data: Bytes::default(),
            mix_hash: B256::default(),
            nonce: FixedBytes::new([0; 8]),
            base_fee_per_gas: None,
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
        };

        // Create a block with no transactions
        let block: Block<TransactionSigned> = Block::new(
            header,
            BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: None,
            },
        );

        // Convert block to payload
        let payload = ExecutionPayloadV1::from_block_slow(&block);
        
        // Convert payload to JSON and then to bytes
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        
        Self { bytes }
    }

    /// Parse the bytes into an ExecutionPayloadV1
    pub fn into_payload(&self) -> Result<ExecutionPayloadV1, serde_json::Error> {
        serde_json::from_slice(&self.bytes)
    }
}

/// Convert a payload to bytes
pub fn payload_to_bytes(payload: &ExecutionPayloadV1) -> Vec<u8> {
    serde_json::to_vec(payload).unwrap_or_default()
}

/// Parse bytes back into a payload
pub fn bytes_to_payload(bytes: &[u8]) -> Result<ExecutionPayloadV1, serde_json::Error> {
    serde_json::from_slice(bytes)
}

fn main() {
    // Example 1: Create a payload from custom bytes
    let custom_bytes = CustomBlockBytes::new();
    let payload = custom_bytes.into_payload().expect("Failed to parse payload");
    println!("Created payload from custom bytes");

    // Example 2: Convert payload to bytes
    let bytes = payload_to_bytes(&payload);
    println!("Converted payload to {} bytes", bytes.len());

    // Example 3: Parse bytes back into payload
    let _parsed_payload = bytes_to_payload(&bytes).expect("Failed to parse bytes");
    println!("Successfully parsed bytes back into payload");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let custom_bytes = CustomBlockBytes::new();
        let payload1 = custom_bytes.into_payload().expect("Failed to parse payload");
        let bytes = payload_to_bytes(&payload1);
        let payload2 = bytes_to_payload(&bytes).expect("Failed to parse bytes");

        // Compare individual fields
        assert_eq!(payload1.parent_hash, payload2.parent_hash, "Parent hash mismatch");
        assert_eq!(payload1.block_number, payload2.block_number, "Block number mismatch");
        assert_eq!(payload1.timestamp, payload2.timestamp, "Timestamp mismatch");
        assert_eq!(payload1.gas_limit, payload2.gas_limit, "Gas limit mismatch");
        assert_eq!(payload1.gas_used, payload2.gas_used, "Gas used mismatch");
        assert_eq!(payload1.base_fee_per_gas, payload2.base_fee_per_gas, "Base fee mismatch");
        
        // Compare transaction lengths
        assert_eq!(
            payload1.transactions.len(),
            payload2.transactions.len(),
            "Transaction count mismatch"
        );

        // Optional: Compare individual transactions if needed
        for (tx1, tx2) in payload1.transactions.iter().zip(payload2.transactions.iter()) {
            assert_eq!(tx1, tx2, "Transaction mismatch");
        }
    }

    #[test]
    fn test_json_equality() {
        let custom_bytes = CustomBlockBytes::new();
        let payload1 = custom_bytes.into_payload().expect("Failed to parse payload");
        let bytes = payload_to_bytes(&payload1);
        let payload2 = bytes_to_payload(&bytes).expect("Failed to parse bytes");

        // Compare JSON representations and log them for debugging
        let json1 = serde_json::to_value(&payload1).expect("Failed to serialize payload1");
        let json2 = serde_json::to_value(&payload2).expect("Failed to serialize payload2");
        println!("JSON 1: {}", serde_json::to_string_pretty(&json1).unwrap());
        println!("JSON 2: {}", serde_json::to_string_pretty(&json2).unwrap());
        assert_eq!(json1, json2, "JSON representations don't match");
    }
} 