# Profile Aggregator

A Nostr relay service that aggregates and verifies user profile information, including profile picture validation and quality filtering.

## Features

- **Profile Event Aggregation**: Collects kind 0 (metadata) and kind 10000 (mute list) events from discovery relays
- **Profile Picture Validation**: Validates profile pictures by downloading and checking image formats
- **Quality Filtering**: Filters low-quality profiles based on configurable criteria
- **Efficient Storage**: Uses LMDB for fast, persistent storage of profile data
- **WebSocket API**: Compatible with standard Nostr relay protocols
- **Configurable Pagination**: Supports paginated profile fetching from discovery relays

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

The service implements the standard Nostr relay protocol over WebSocket:

- `ws://localhost:8080` - WebSocket endpoint for Nostr protocol
- `http://localhost:8080/health` - Health check endpoint

### Supported NIPs

- NIP-01: Basic protocol flow
- NIP-09: Event deletion
- NIP-11: Relay information document
- NIP-40: Expiration timestamp
- NIP-42: Authentication (optional)
- NIP-70: Protected events

## Architecture

The profile aggregator consists of several components:

1. **Discovery Service**: Connects to configured relay and fetches profile events
2. **Image Validator**: Downloads and validates profile pictures
3. **Quality Filter**: Applies quality criteria to filter profiles
4. **Storage Layer**: Persists profiles in LMDB database
5. **WebSocket Server**: Serves filtered profiles via Nostr protocol

## Development

### Running Tests

```bash
cargo test
```

### Building Docker Image

```bash
docker build -t profile_aggregator .
```

### Running with Docker Compose

```bash
docker compose up -d
```

## Deployment

The service can be deployed using Ansible. See the `ansible/` directory for deployment playbooks and configuration.

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.