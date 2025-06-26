# Debugging Graceful Shutdown Issues with Arc<RelayDatabase>

## Problem Description
The ingester loop in `nostr-lmdb` won't exit until all `Arc<RelayDatabase>` instances are dropped. This is preventing graceful shutdown.

## Setup for Debugging

### 1. Install tokio-console
```bash
cargo install --locked tokio-console
```

### 2. Run the debug script
```bash
./debug_console.sh
```

### 3. In another terminal, connect to tokio-console
```bash
tokio-console
```

## What to Look For in tokio-console

### Tasks View (default)
- Look for tasks that are still running after Ctrl+C
- Check task names and their state
- Look for tasks with high poll counts that might be spinning

### Resources View (press 'r')
- Check for Arc references and their counts
- Look for resources that aren't being dropped

### Key Commands in tokio-console
- `↑/↓` - Navigate tasks
- `Enter` - View task details
- `r` - Switch to resources view
- `t` - Switch back to tasks view
- `q` - Quit

## Common Causes of Arc<RelayDatabase> Not Being Dropped

1. **Background tasks holding references**
   - Profile aggregation service
   - Validation tasks
   - WebSocket connections

2. **Circular references**
   - Check if any service holds a reference that creates a cycle

3. **Forgotten clones**
   - Look for places where database is cloned but not properly cleaned up

## Additional Debugging Steps

### 1. Add manual Arc tracking
Add this to your main.rs to track Arc count:
```rust
// After creating the database Arc
let db_weak = Arc::downgrade(&database);
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("Arc<RelayDatabase> strong count: {}", db_weak.strong_count());
        if db_weak.strong_count() == 0 {
            println!("All Arc<RelayDatabase> instances dropped!");
            break;
        }
    }
});
```

### 2. Check specific services
Look for these patterns in the code:
- `database.clone()` without corresponding drop
- `tokio::spawn` with database captures
- Long-running loops that hold database references

### 3. Use RUST_LOG for detailed logging
```bash
export RUST_LOG=debug,nostr_lmdb=trace,profile_aggregator=trace
```

## Fixing Common Issues

### 1. Ensure all spawned tasks can be cancelled
```rust
let handle = tokio::spawn({
    let token = cancellation_token.clone();
    async move {
        tokio::select! {
            _ = token.cancelled() => {
                // Cleanup
            }
            _ = some_work() => {
                // Normal operation
            }
        }
    }
});
```

### 2. Use weak references where appropriate
```rust
let db_weak = Arc::downgrade(&database);
tokio::spawn(async move {
    while let Some(db) = db_weak.upgrade() {
        // Use db
    }
});
```

### 3. Ensure proper cleanup in drop handlers
Check if any services need explicit cleanup in their Drop implementations.

## Testing Shutdown

1. Start the application with debug script
2. Let it run for a few seconds
3. Press Ctrl+C
4. Watch tokio-console for tasks that don't exit
5. Check logs for "Ingester thread exited" message

If the ingester doesn't exit, there's still an Arc<RelayDatabase> reference somewhere.