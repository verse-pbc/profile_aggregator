# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Common Development Commands

### Building and Running
- `cargo build` - Build in debug mode
- `cargo build --release` - Build optimized release binary
- `cargo run` - Run in debug mode
- `cargo run --release` - Run in release mode
- `./test_run.sh` - Run with test configuration (uses local data directory)

### Testing
- `cargo test` - Run all tests
- `cargo test -- --nocapture` - Run tests with print output
- `cargo test <test_name>` - Run specific test by name

### Code Quality
- `cargo fmt` - Format code (required by pre-commit hook)
- `cargo fmt -- --check` - Check formatting without changing files
- `cargo clippy --all-targets -- -D warnings` - Run linter (required by pre-commit hook)
- `cargo check` - Fast type checking

### Docker
- `docker build -t profile_aggregator .` - Build Docker image
- `docker run -p 8080:8080 -v ./data:/data profile_aggregator` - Run with persistent storage

## Architecture Overview

Profile Aggregator is a specialized Nostr relay that harvests, validates, and serves high-quality user profiles. It acts as both a profile aggregation service and a standard Nostr relay.

### Core Components

1. **Main Service (`main.rs`)**
   - WebSocket endpoint on port 8080 for Nostr protocol
   - HTTP health check at `/health`
   - Uses LMDB for event storage
   - Integrates all components via `nostr_relay_builder`

2. **Profile Aggregation Service (`profile_aggregation_service.rs`)**
   - Manages harvesters that fetch profiles from external relays
   - Implements pagination with resumable state
   - Uses exponential backoff for rate limiting (initial: 60s, max: 1800s)
   - Processes events sequentially to avoid overwhelming validators

3. **Profile Quality Filter (`profile_quality_filter.rs`)**
   - Validates profiles must have:
     - Name (display_name or name field)
     - Non-empty bio (about field)
     - Valid profile picture URL
   - Excludes bridge accounts (mostr) and ActivityPub profiles
   - Implements `EventProcessor` trait for relay integration

4. **Profile Validator (`profile_validator.rs`)**
   - Verifies profiles have published text notes (kind 1)
   - Uses outbox/gossip protocol for verification
   - Implements retry logic with rate limiting
   - Tracks validation metrics in LMDB

5. **Image Validator (`profile_image_validator.rs`)**
   - Downloads and validates profile images
   - Requires minimum dimensions: 300x600px
   - Supports multiple formats via `image` crate
   - Implements per-domain rate limiting

6. **Relay List Collector (`relay_list_collector.rs`)**
   - Fetches relay lists (kinds 10002, 10050) for accepted profiles
   - Uses outbox model to find user's relays
   - Stores collected lists in local database

### Data Flow

1. **Discovery Phase**: Harvesters connect to external relays and fetch metadata events (kind 0)
2. **Validation Pipeline**:
   - Basic profile validation (name, bio, picture URL)
   - Image download and dimension validation
   - Text note verification via gossip protocol
   - Relay list collection for accepted profiles
3. **Storage & Serving**: Validated profiles and relay lists stored in LMDB, served via standard Nostr relay interface

### Key Design Patterns

- **Middleware Architecture**: Uses `nostr_relay_builder` middleware pattern
- **Event Processing**: Implements `EventProcessor` trait for filtering
- **Rate Limiting**: Per-domain rate limiting with `governor` crate
- **Async Processing**: Tokio-based async throughout
- **Resumable State**: Pagination state persisted in LMDB

## Environment Variables

- `DISCOVERY_RELAY_URL` - Source relay URL (default: wss://relay.nos.social)
- `BIND_ADDR` - Server bind address (default: 0.0.0.0:8080)
- `DATABASE_PATH` - LMDB storage path (default: /data/profile_aggregator.db)
- `PAGE_SIZE` - Events per pagination request (default: 500)
- `RELAY_SECRET_KEY` - Relay's Nostr private key (auto-generated if not set)
- `INITIAL_BACKOFF_SECS` - Initial retry backoff (default: 60)
- `MAX_BACKOFF_SECS` - Maximum retry backoff (default: 1800)
- `RUST_LOG` - Log configuration (default: profile_aggregator=info)

## Testing Strategy

- Unit tests alongside implementation files
- Integration tests in `tests/` directory
- Mock external dependencies (relays, HTTP requests)
- Test both success and failure paths
- Verify rate limiting and backoff behavior

## Important Notes

1. **Rust Version**: Pinned to 1.87.0 via `rust-toolchain.toml`
2. **Pre-commit Hooks**: Automatically run `cargo fmt` and `cargo clippy`
3. **Sequential Processing**: Events processed one at a time to respect rate limits
4. **Image Validation**: Full download required to verify dimensions
5. **Gossip Protocol**: Used for outbox model verification
6. **Database Keys**: 
   - Profile states: `profile_state:{pubkey}`
   - Pagination state: `pagination_state:{relay_url}`
   - Validation state: `validation_state:{pubkey}`