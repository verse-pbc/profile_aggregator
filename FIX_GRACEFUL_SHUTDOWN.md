# Fix for Graceful Shutdown Issue

## Problem
The blocking tasks spawned by `RelayDatabase` are holding `Arc<NostrLMDB>` references and waiting on channel receivers. They won't exit until all senders are dropped, but the senders are held within the closure of other tasks, creating a dependency cycle.

## Root Cause
1. `CryptoWorker` spawns blocking tasks that wait on `rx.recv()` 
2. `RelayDatabase` spawns blocking tasks for save/delete operations that wait on `receiver.recv()`
3. These tasks hold `Arc<NostrLMDB>` references
4. The nostr-lmdb ingester waits for all `Arc<RelayDatabase>` to be dropped
5. But `RelayDatabase` contains `Arc<NostrLMDB>`, creating circular dependency

## Solution

### Option 1: Add Cancellation Token to Blocking Tasks (Recommended)
Modify the blocking tasks to check for cancellation:

```rust
// In crypto_worker.rs around line 111
task_tracker.spawn_blocking({
    let cancellation_token = cancellation_token.clone();
    move || {
        debug!("Crypto worker {} started", i);
        
        loop {
            // Check for cancellation
            if cancellation_token.is_cancelled() {
                debug!("Crypto worker {} shutting down", i);
                break;
            }
            
            // Use try_recv with timeout instead of blocking recv
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(op) => {
                    // Process operation
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    debug!("Crypto worker {} channel closed", i);
                    break;
                }
            }
        }
    }
});
```

### Option 2: Ensure Senders are Dropped on Shutdown
Store the senders in RelayDatabase and explicitly drop them:

```rust
pub struct RelayDatabase {
    lmdb: Arc<NostrLMDB>,
    broadcast_sender: broadcast::Sender<Box<Event>>,
    queue_capacity: usize,
    // Add these to allow explicit dropping
    _command_sender: Option<flume::Sender<CommandWithReply>>,
    _save_signed_sender: Option<flume::Sender<SaveSignedItem>>,
    _save_unsigned_sender: Option<flume::Sender<SaveUnsignedItem>>,
    _delete_sender: Option<flume::Sender<DeleteItem>>,
}

impl Drop for RelayDatabase {
    fn drop(&mut self) {
        // Explicitly drop senders to signal shutdown
        self._command_sender.take();
        self._save_signed_sender.take();
        self._save_unsigned_sender.take();
        self._delete_sender.take();
    }
}
```

### Option 3: Use Weak References in Blocking Tasks
Convert `Arc<NostrLMDB>` to `Weak<NostrLMDB>` in blocking tasks:

```rust
fn spawn_save_signed_processor(
    receiver: flume::Receiver<SaveSignedItem>,
    env: Arc<NostrLMDB>,
    broadcast_sender: broadcast::Sender<Box<Event>>,
    task_tracker: &TaskTracker,
) {
    let env_weak = Arc::downgrade(&env);
    
    task_tracker.spawn_blocking(move || {
        info!("Save signed processor started");
        
        loop {
            let first_item = match receiver.recv() {
                Ok(item) => item,
                Err(_) => {
                    debug!("Save signed processor channel closed");
                    break;
                }
            };
            
            // Try to upgrade weak reference
            let Some(env) = env_weak.upgrade() else {
                debug!("Database dropped, exiting processor");
                break;
            };
            
            // Process batch...
        }
    });
}
```

## Immediate Workaround

Add explicit Arc tracking in main.rs to debug:

```rust
// After creating the database Arc
let db_weak = Arc::downgrade(&database);
let debug_tracker = task_tracker.spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let strong_count = db_weak.strong_count();
        if strong_count > 0 {
            info!("Arc<RelayDatabase> strong count: {}", strong_count);
        } else {
            info!("All Arc<RelayDatabase> instances dropped!");
            break;
        }
    }
});

// During shutdown
info!("Waiting for background tasks...");
// First wait a bit for Arc cleanup
tokio::time::sleep(Duration::from_millis(100)).await;

match tokio::time::timeout(Duration::from_secs(30), task_tracker.wait()).await {
    // ...
}
```

## Testing the Fix

1. Run with tokio-console enabled
2. Trigger shutdown with Ctrl+C
3. Verify all blocking tasks exit
4. Check that "Ingester thread exited" appears in logs
5. Confirm clean shutdown without timeout

## Long-term Recommendation

The blocking tasks should be refactored to:
1. Use async tasks with proper cancellation support
2. Or use `recv_timeout` with periodic cancellation checks
3. Store channel senders in a way that allows explicit dropping during shutdown