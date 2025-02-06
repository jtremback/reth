# Payload Parsing Example

This example demonstrates how to:

1. Parse custom block bytes into Reth's execution payload format
2. Convert payloads to and from bytes using RLP encoding
3. Work with Reth's transaction and block types

## Key Components

-   `CustomBlockBytes`: Example struct showing how to wrap your chain's raw block bytes
-   `into_payload()`: Shows conversion from custom format to `ExecutionPayloadV1`
-   `payload_to_bytes()`: Demonstrates serializing a payload to bytes
-   `bytes_to_payload()`: Shows how to parse bytes back into a payload

## Running the Example

From the root of the Reth repository:

```bash
cargo run --example payload-parsing
```

## Testing

The example includes a test demonstrating roundtrip serialization:

```bash
cargo test --example payload-parsing
```

## Key Points

1. The example uses `ExecutionPayloadV1` which is the simplest payload format
2. RLP encoding is used for byte serialization
3. The example shows how to create transactions and blocks using Reth's types
4. Error handling is included via anyhow

## Customizing for Your Chain

To adapt this for your chain:

1. Modify `CustomBlockBytes` to match your chain's block format
2. Implement the parsing logic in `into_payload()` to extract:
    - Block header fields (timestamp, number, etc.)
    - Transactions
    - Other required fields
3. Adjust the serialization format in `payload_to_bytes()` if needed

## Notes

-   This is a minimal example focused on serialization
-   In a real implementation, you'd need to:
    -   Add proper validation
    -   Handle all transaction types
    -   Implement proper error types
    -   Add more comprehensive tests
