# Profile Aggregator

A Nostr relay that aggregates high-quality user profiles from the network.

## What it does

- Discovers profiles from other relays
- Validates profile quality (bio, image size, real activity)
- Filters out bots and low-quality accounts
- Serves curated profiles via WebSocket

## Quick Start

```bash
# Docker
docker run -p 8080:8080 ghcr.io/verse-pbc/profile_aggregator:latest

# From source
cargo run --release
```

## Configuration

Key environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `DISCOVERY_RELAY_URL` | Source relay for profiles | `wss://relay.nos.social` |
| `BIND_ADDR` | Server address | `0.0.0.0:8080` |
| `WORKER_THREADS` | Validation workers | `4` |
| `DATABASE_PATH` | Data storage | `/data/profile_aggregator.db` |
| `RUST_LOG` | Log level | `profile_aggregator=info` |

## Development

```bash
# Install git hooks
./scripts/setup-hooks.sh

# Run tests
cargo test

# Build
cargo build --release
```

## API

- `ws://localhost:8080` - Nostr WebSocket
- `http://localhost:8080/health` - Health check

## License

MIT