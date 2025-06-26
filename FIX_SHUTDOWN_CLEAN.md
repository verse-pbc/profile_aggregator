# Clean Shutdown Fix - Dropping All DatabaseSender Instances

## Problem
The blocking tasks in `CryptoWorker` and `RelayDatabase` are waiting on channel receivers. They will exit when all senders are dropped, but multiple components hold clones of `DatabaseSender`:

1. **Main thread**: Drops its copy on line 796
2. **ProfileAggregationService**: Holds a copy via `ProfileValidator` 
3. **WebSocket handler**: Holds a copy via `RelayConfig`

## Root Cause Analysis

### Sender Chain
```
main.rs creates DatabaseSender
  ├── Cloned to ProfileAggregationService
  │   └── Stored in ProfileValidator.db_sender
  ├── Cloned to RelayConfig 
  │   └── Used by WebSocket handler
  └── Local copy dropped on line 796
```

### Why Shutdown Hangs
- Blocking tasks wait on `receiver.recv()`
- Channel only closes when ALL senders are dropped
- ProfileAggregationService task holds a sender even after cancellation
- WebSocket handler might hold references in active connections

## Solution

### Option 1: Explicit Drop in Aggregation Service (Quick Fix)
Modify the aggregation service task to explicitly drop on cancellation:

```rust
// In main.rs around line 725
let aggregation_handle = task_tracker.spawn(async move {
    info!("🔄 Starting profile aggregation service...");
    let result = tokio::select! {
        result = aggregation_service.run() => {
            result
        }
        _ = aggregation_service_token.cancelled() => {
            info!("Profile aggregation service cancelled");
            Ok(())
        }
    };
    
    // Explicitly drop to release DatabaseSender
    drop(aggregation_service);
    
    if let Err(e) = result {
        error!("Profile aggregation service error: {}", e);
    }
});
```

### Option 2: Use Weak References in Components
Modify components to use weak references or not store the sender:

```rust
// In ProfileValidator
pub struct ProfileValidator {
    filter: Arc<ProfileQualityFilter>,
    db_sender: Option<DatabaseSender>, // Make optional
    // ... other fields
}

impl ProfileValidator {
    pub fn shutdown(&mut self) {
        self.db_sender = None; // Drop the sender
    }
}
```

### Option 3: Shutdown Coordinator (Recommended)
Add a shutdown method to properly clean up all resources:

```rust
// Add to main.rs after server stops (line 793)
info!("Server stopped, shutting down services...");

// Signal all services to shutdown
cancellation_token.cancel();

// Wait briefly for services to respond to cancellation
tokio::time::sleep(Duration::from_millis(100)).await;

// Clean up resources in order
drop(ws_handler);  // Drop WebSocket handler first
drop(config);      // Drop config (contains DatabaseSender)
drop(filter);      // Drop filter
drop(db_sender);   // Drop our local copy

// Now wait for tasks
info!("Waiting for background tasks...");
```

### Option 4: Track All Clones (Best Long-term)
Refactor to track all DatabaseSender clones:

```rust
// Create a wrapper that tracks clones
pub struct TrackedDatabaseSender {
    inner: DatabaseSender,
    _guard: Arc<()>, // Dropped when sender is dropped
}

// In main, track when all are dropped
let (tracker, guard) = Arc::new(()).into_raw_parts();
let tracked_sender = TrackedDatabaseSender {
    inner: db_sender,
    _guard: Arc::from_raw(guard),
};

// During shutdown, wait for all guards to drop
while Arc::strong_count(&tracker) > 1 {
    tokio::time::sleep(Duration::from_millis(10)).await;
}
```

## Immediate Fix

The quickest fix is to ensure the aggregation service drops its resources:

```rust
// Modify main.rs line 725-737
let aggregation_handle = task_tracker.spawn({
    let mut service = aggregation_service; // Take ownership
    async move {
        info!("🔄 Starting profile aggregation service...");
        tokio::select! {
            result = service.run() => {
                if let Err(e) = result {
                    error!("Profile aggregation service error: {}", e);
                }
            }
            _ = aggregation_service_token.cancelled() => {
                info!("Profile aggregation service cancelled");
            }
        }
        // Service dropped here, releasing DatabaseSender
    }
});
```

## Testing
1. Run with tokio-console
2. Trigger shutdown with Ctrl+C
3. Verify "Ingester thread exited" appears in logs
4. Confirm no timeout on shutdown

## Long-term Recommendations
1. Components should not store DatabaseSender if they don't need it long-term
2. Use channels or oneshot for individual operations instead
3. Consider a shutdown coordinator that explicitly drops resources in order
4. Add Drop implementations that log when key resources are released