ARG RUST_VERSION=1.86.0

FROM rust:${RUST_VERSION}-slim-bookworm AS rust-builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/app

# Copy the entire workspace for building
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build the profile_aggregator binary
RUN cargo build --release -p profile_aggregator

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary
COPY --from=rust-builder /usr/src/app/target/release/profile_aggregator ./profile_aggregator

# Create data directory
RUN mkdir -p ./data

EXPOSE 8080

# Environment variables that can be overridden
ENV RUST_LOG=info,profile_aggregator=debug,nostr_relay_builder=debug
ENV RELAY_URL=ws://localhost:8080
ENV DISCOVERY_RELAY_URL=wss://relay.nos.social
ENV BIND_ADDR=0.0.0.0:8080
ENV RELAY_CONTACT=admin@relay.example
ENV RELAY_SECRET_KEY=339e1ab1f59eb304b8cb5202eddcc437ff699fc523161b6e2c222590cccb3b84
ENV PAGE_SIZE=500
ENV INITIAL_BACKOFF_SECS=2
ENV MAX_BACKOFF_SECS=300
ENV WORKER_THREADS=20
ENV STATE_FILE=./data/aggregation_state.json
ENV DATABASE_PATH=./data/profile_aggregator.db

CMD ["./profile_aggregator"]