# Graceful Shutdown Improvements

This document describes the improvements made to ensure proper graceful shutdown of the profile aggregator service.

## Problem
- WebSocket connections from nostr-sdk clients weren't being properly closed during shutdown
- The gossip client in ProfileValidator was not being explicitly shut down
- Duplicate CryptoWorker instances were being created, preventing clean shutdown
- Handler closures in the router were holding database references

## Solution

### 1. Added shutdown method to ProfileValidator
```rust
/// Shutdown the validator and its resources
pub async fn shutdown(&self) {
    debug!("Shutting down profile validator");
    self.gossip_client.shutdown().await;
}
```

### 2. Added shutdown method to ProfileAggregationService
```rust
/// Shutdown the aggregation service and its resources
pub async fn shutdown(&self) {
    info!("Shutting down profile aggregation service");
    
    // Shutdown the validator which will close the gossip client
    self.validator.shutdown().await;
    
    // Final state save
    let state_snapshot = self.state.read().await;
    if let Err(e) = Self::save_state(&state_snapshot, &self.config.state_file).await {
        error!("Failed to save final aggregation state: {}", e);
    }
}
```

### 3. Updated main.rs shutdown sequence
- Store aggregation service in Arc for shutdown
- Call `aggregation_service.shutdown().await` after cancellation
- Drop resources in proper order (ws_handler, config, aggregation_service, db_sender)
- Extended wait time to 1 second for connections to close

### 4. Fixed RelayBuilder crypto worker creation
- RelayBuilder now always creates a crypto_sender for signature verification middleware
- This results in duplicate CryptoWorker instances when using an existing database
- Each CryptoWorker spawns 3 blocking threads (6 total)

## Remaining Issues

### Duplicate CryptoWorker Problem
When passing an existing database instance to RelayBuilder:
1. main.rs creates a CryptoWorker for the database
2. RelayBuilder creates another CryptoWorker for signature verification
3. This results in 6 blocking threads that all wait on flume channels
4. All these threads must exit before the TaskTracker can complete

### Handler Closure References
The `/random` route handler captures the database Arc, which internally holds a DatabaseSender. This reference is held by the router even after the server stops.

## Potential Solutions

1. **Let RelayBuilder manage everything**: Don't create the database in main.rs, let RelayBuilder create both the database and crypto worker
2. **Modify RelayBuilder**: Make it reuse the crypto worker from an existing database instance
3. **Add explicit shutdown to CryptoWorker**: Implement a shutdown mechanism for the flume channels

## Testing
Use `./debug_console.sh` to run with tokio-console enabled, then:
1. Let it run for a bit to establish connections
2. Press Ctrl+C to trigger shutdown
3. Check tokio-console to verify task cleanup
4. Look for remaining blocking tasks from crypto workers and ingester