# Profile Aggregator

A specialized Nostr relay built with [nostr_relay_builder](https://github.com/verse-pbc/groups_relay/tree/main/crates/nostr_relay_builder) that aggregates high-quality user profiles.

## Features

- Fetches kind 0 (metadata) events from discovery relays
- Validates profiles have published content (kind 1) via outbox verification
- Filters low-quality profiles (missing bio, small images, bot accounts)
- Discovers and stores user relay preferences (kinds 10002, 10050)
- Validates profile pictures (minimum 300x600px, excludes placeholders)

## Installation

### Using Docker

```bash
docker pull ghcr.io/verse-pbc/profile_aggregator:latest
docker run -p 8080:8080 ghcr.io/verse-pbc/profile_aggregator:latest
```

### Building from Source

```bash
# Clone the repository
git clone https://github.com/verse-pbc/profile_aggregator.git
cd profile_aggregator

# Install git hooks
./scripts/setup-hooks.sh

# Build the project
cargo build --release

# Run the service
cargo run --release
```

## Configuration

The service is configured through environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `RELAY_URL` | Public URL of this relay | `https://localhost:8080` |
| `RELAY_CONTACT` | Contact information for relay operator | `admin@localhost` |
| `RELAY_SECRET_KEY` | Relay's secret key (hex format) | Generated if not provided |
| `DISCOVERY_RELAY_URL` | Relay to fetch profiles from | `wss://relay.nos.social` |
| `BIND_ADDR` | Address to bind the server to | `0.0.0.0:8080` |
| `PAGE_SIZE` | Number of profiles to fetch per page | `500` |
| `INITIAL_BACKOFF_SECS` | Initial backoff for retries | `60` |
| `MAX_BACKOFF_SECS` | Maximum backoff for retries | `3600` |
| `WORKER_THREADS` | Number of worker threads | `4` |
| `DATABASE_PATH` | Path to LMDB database | `/data/profile_aggregator.db` |
| `STATE_FILE` | Path to state persistence file | `/data/state.json` |
| `RUST_LOG` | Logging configuration | `profile_aggregator=info` |

## Docker Compose Example

```yaml
version: '3.8'

services:
  profile_aggregator:
    image: ghcr.io/verse-pbc/profile_aggregator:latest
    container_name: profile_aggregator
    restart: unless-stopped
    environment:
      - RELAY_URL=https://profiles.example.com
      - RELAY_CONTACT=admin@example.com
      - DISCOVERY_RELAY_URL=wss://relay.nos.social
      - RUST_LOG=profile_aggregator=info,nostr_relay_builder=info
    volumes:
      - ./data:/data
    ports:
      - "8080:8080"
```

## API Endpoints

- `ws://localhost:8080` - WebSocket endpoint
- `http://localhost:8080/health` - Health check

## How it Works

1. Connects to discovery relay and fetches metadata events
2. For each profile, validates:
   - Has name and bio
   - Profile picture is valid and large enough
   - User has published at least one text note (outbox verification)
   - Not a bot or bridge account
3. Stores accepted profiles and relay preferences in LMDB
4. Serves filtered profiles via standard Nostr relay protocol

## Development

```bash
cargo test
cargo build --release
docker build -t profile_aggregator .
```

## Deployment

Deploy with Ansible:

```bash
ansible-playbook -i inventories/profile_aggregator/inventory.yml playbooks/profile_aggregator.yml --ask-vault-pass
```

## License

MIT License