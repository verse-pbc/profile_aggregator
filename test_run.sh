#!/bin/bash

# Create data directory if it doesn't exist
mkdir -p ./data

# Set environment variables
export RUST_LOG='info'
export RELAY_URL=ws://localhost:8080
export BIND_ADDR=127.0.0.1:8080
export RELAY_CONTACT=admin@relay.example
export CONSUMER_RELAY_URL=wss://relay.nos.social

echo "🚀 Starting Profile Quality Relay Server..."
echo "📁 Data will be stored in ./data/"
echo ""

# Run the server
cargo run --release
