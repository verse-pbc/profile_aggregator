# Profile Aggregator

A Nostr relay that aggregates and filters user profiles from the network.

## What it does

- Harvests profiles from external relays
- Validates quality (bio, image validity, spam filtering)
- Serves filtered profiles via standard Nostr protocol
- All events (harvested or client-submitted) go through the same filter

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
| `WORKER_THREADS` | Validation workers | `1` |
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

- `ws://localhost:8080` - Nostr relay endpoint
- `http://localhost:8080/health` - Health check

## License

MIT