#!/bin/bash

# Script to run profile_aggregator with tokio-console enabled
# This will help debug Arc<RelayDatabase> instances that are not being dropped

echo "🔍 Starting profile_aggregator with tokio-console enabled..."
echo ""
echo "📊 tokio-console will be available on port 6669"
echo "📱 To connect, run in another terminal: tokio-console"
echo ""
echo "⚡ Key things to look for in tokio-console:"
echo "   1. Long-running tasks that might be holding Arc<RelayDatabase>"
echo "   2. Tasks that are not completing on shutdown"
echo "   3. Resource usage patterns"
echo ""
echo "🛑 Press Ctrl+C to shutdown and observe the cleanup process"
echo ""

# Set environment variables for tokio-console
export TOKIO_CONSOLE=1

# Set logging for shutdown debugging without excessive tokio trace info
export RUST_LOG=info,profile_aggregator=debug,nostr_relay_builder=info,nostr_lmdb::store::ingester=info

# Run with unstable features required by tokio-console
RUSTFLAGS="--cfg tokio_unstable" cargo run --release

echo ""
echo "✅ Shutdown complete. Check if all tasks were properly cleaned up."