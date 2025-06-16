use anyhow::Result;
use axum::{routing::get, Router};
use nostr_relay_builder::{CryptoWorker, RelayBuilder, RelayConfig, RelayDatabase, RelayInfo};
use nostr_sdk::prelude::*;
use profile_aggregator::{
    ProfileAggregationConfig, ProfileAggregationService, ProfileQualityFilter,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables
    dotenv::dotenv().ok();

    // Setup logging
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,profile_aggregator=debug,nostr_relay_builder=debug")
    });
    fmt().with_env_filter(env_filter).with_target(true).init();

    // Load relay keys from environment or use default dev key
    let relay_secret = std::env::var("RELAY_SECRET_KEY").unwrap_or_else(|_| {
        "339e1ab1f59eb304b8cb5202eddcc437ff699fc523161b6e2c222590cccb3b84".to_string()
    });

    let keys = Keys::parse(&relay_secret)?;
    println!("🔑 Relay public key: {}", keys.public_key());

    // Create the crypto worker and database
    let cancellation_token = CancellationToken::new();
    let crypto_worker = Arc::new(CryptoWorker::new(
        Arc::new(keys.clone()),
        cancellation_token.clone(),
    ));
    let database_path = std::env::var("DATABASE_PATH")
        .unwrap_or_else(|_| "./data/profile_aggregator.db".to_string());

    let database = Arc::new(RelayDatabase::new(&database_path, crypto_worker)?);

    // Configure the relay
    let relay_url =
        std::env::var("RELAY_URL").unwrap_or_else(|_| "ws://localhost:8080".to_string());
    let config = RelayConfig::new(&relay_url, database.clone(), keys);

    // Get discovery relay URLs
    let discovery_relay_urls = vec![std::env::var("DISCOVERY_RELAY_URL")
        .unwrap_or_else(|_| "wss://relay.nos.social".to_string())];

    // Configure profile aggregation service to only fetch metadata events
    let page_size: usize = std::env::var("PAGE_SIZE")
        .unwrap_or_else(|_| "500".to_string())
        .parse()
        .unwrap_or(500);

    let initial_backoff_secs: u64 = std::env::var("INITIAL_BACKOFF_SECS")
        .unwrap_or_else(|_| "2".to_string())
        .parse()
        .unwrap_or(2);

    let max_backoff_secs: u64 = std::env::var("MAX_BACKOFF_SECS")
        .unwrap_or_else(|_| "300".to_string())
        .parse()
        .unwrap_or(300);

    let state_file =
        std::env::var("STATE_FILE").unwrap_or_else(|_| "./data/aggregation_state.json".to_string());

    let aggregation_config = ProfileAggregationConfig {
        relay_urls: discovery_relay_urls.clone(),
        filters: vec![Filter::new().kind(Kind::Metadata)],
        page_size,
        state_file: PathBuf::from(state_file.clone()),
        initial_backoff: Duration::from_secs(initial_backoff_secs),
        max_backoff: Duration::from_secs(max_backoff_secs),
    };

    // Create shared filter for both WebSocket and aggregation service
    let filter = Arc::new(ProfileQualityFilter::new(database.clone()));

    // Both services use the same filter
    let aggregation_service =
        ProfileAggregationService::new(aggregation_config, filter.clone(), database.clone())
            .await
            .expect("Failed to create aggregation service");

    // Create relay info for NIP-11
    let relay_info = RelayInfo {
        name: "Profile Aggregator Relay".to_string(),
        description: "A relay that aggregates and filters user profiles".to_string(),
        pubkey: config.keys.public_key().to_hex(),
        contact: std::env::var("RELAY_CONTACT")
            .unwrap_or_else(|_| "admin@relay.example".to_string()),
        supported_nips: vec![1, 11],
        software: "profile_aggregator".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        icon: None,
    };

    // Build the WebSocket server
    let root_handler = RelayBuilder::new(config.clone())
        .build_axum_handler(filter.as_ref().clone(), relay_info)
        .await?;

    // Create HTTP server
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(|| async { "OK" }));

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind_addr.parse()?;

    // Spawn the aggregation service
    let aggregation_service_token = cancellation_token.clone();
    let aggregation_handle = tokio::spawn(async move {
        info!("🔄 Starting profile aggregation service...");
        tokio::select! {
            result = aggregation_service.run() => {
                if let Err(e) = result {
                    error!("Profile aggregation service error: {}", e);
                }
            }
            _ = aggregation_service_token.cancelled() => {
                info!("Profile aggregation service cancelled");
            }
        }
    });

    println!("\nProfile Aggregator starting");
    println!("WebSocket: ws://{}", addr);
    println!("\nFetching user profiles (kind 0)");
    println!("\nDiscovery relay: {}", discovery_relay_urls.join(", "));
    println!("\nProfile requirements:");
    println!("- Name: display_name or name field");
    println!("- Bio: non-empty about field");
    println!("- Picture: valid URL, min 300x600px");
    println!("- Verified: published text note via outbox relays");
    println!("- Excludes: bridges, mostr accounts, profiles with fields array");
    println!("\nConfiguration:");
    println!("- Page size: {} events", page_size);
    println!("- Backoff: {}s-{}s", initial_backoff_secs, max_backoff_secs);
    println!("- Database: {}", database_path);
    println!("- State: {}", state_file);

    // Handle shutdown signal
    let shutdown_token = cancellation_token.clone();
    let shutdown_signal = async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        info!("Shutdown signal received, stopping services...");
        shutdown_token.cancel();
    };

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Run server with graceful shutdown
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal);

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                error!("Server error: {}", e);
            }
        }
        _ = aggregation_handle => {
            info!("Profile aggregation service stopped");
        }
        _ = cancellation_token.cancelled() => {
            info!("Cancellation requested");
        }
    }

    info!("Server stopped");

    Ok(())
}
