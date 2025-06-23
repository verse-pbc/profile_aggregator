use anyhow::Result;
use axum::{
    extract::{ConnectInfo, Query},
    http::HeaderMap,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use nostr_relay_builder::{CryptoWorker, RelayBuilder, RelayConfig, RelayDatabase, RelayInfo};
use nostr_sdk::prelude::*;
use profile_aggregator::{
    ProfileAggregationConfig, ProfileAggregationService, ProfileQualityFilter,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

// HTML templates
static INDEX_TEMPLATE: &str = include_str!("../templates/index.html");
static RANDOM_PROFILES_TEMPLATE: &str = include_str!("../templates/random_profiles.html");

fn render_index_html(info: &RelayInfo, websocket_url: &str) -> String {
    let supported_nips = info
        .supported_nips
        .iter()
        .map(|nip| format!(r#"<span class="nip">NIP-{:02}</span>"#, nip))
        .collect::<Vec<_>>()
        .join("\n                    ");

    INDEX_TEMPLATE
        .replace("{title}", &info.name)
        .replace("{name}", &info.name)
        .replace("{description}", &info.description)
        .replace("{websocket_url}", websocket_url)
        .replace("{pubkey}", &info.pubkey)
        .replace("{contact}", &info.contact)
        .replace("{software}", &info.software)
        .replace("{version}", &info.version)
        .replace("{supported_nips}", &supported_nips)
}

fn render_random_profiles_html(profiles: &[ProfileWithRelays]) -> String {
    let profiles_json = serde_json::to_string(profiles).unwrap_or_default();
    let profile_count = *PROFILE_COUNT.read().unwrap();

    RANDOM_PROFILES_TEMPLATE
        .replace("{profile_count}", &profile_count.to_string())
        .replace("{profiles_json}", &profiles_json)
}

#[derive(Deserialize)]
struct RandomQuery {
    #[serde(default = "default_count")]
    count: usize,
}

fn default_count() -> usize {
    12
}

#[derive(Serialize)]
struct ProfileWithRelays {
    profile: Event,
    #[serde(skip_serializing_if = "Option::is_none")]
    relay_list: Option<Event>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dm_relay_list: Option<Event>,
}

// Global state for oldest timestamp
static OLDEST_TIMESTAMP: RwLock<Option<u64>> = RwLock::new(None);
// Global state for profile count
static PROFILE_COUNT: RwLock<usize> = RwLock::new(0);

async fn find_oldest_timestamp(database: &Arc<RelayDatabase>) -> Result<u64> {
    info!("Finding oldest timestamp using binary search...");

    let scope = nostr_lmdb::Scope::Default;
    let now = Timestamp::now().as_u64();
    const YEAR_SECONDS: u64 = 365 * 24 * 60 * 60;

    // Start by going back year by year to find a period with no events
    let mut years_back = 1;
    let mut found_empty_period = false;
    let mut last_year_with_events = 0;

    loop {
        let until = Timestamp::from(now.saturating_sub((years_back - 1) * YEAR_SECONDS));

        // Use only until to check if any events exist before this timestamp
        let filter = Filter::new().kind(Kind::Metadata).until(until).limit(1);

        match database.query(vec![filter], &scope).await {
            Ok(events) => {
                if events.into_iter().next().is_some() {
                    // Found events, go back another year
                    info!("Found events {} years back, going further...", years_back);
                    last_year_with_events = years_back;
                    years_back += 1;
                } else {
                    // No events before this timestamp
                    info!("No events found {} years back", years_back);
                    found_empty_period = true;
                    break;
                }
            }
            Err(e) => {
                error!("Error querying for oldest timestamp: {}", e);
                return Err(e.into());
            }
        }

        // Safety limit
        if years_back > 10 {
            info!("Reached 10 years back, stopping search");
            break;
        }
    }

    // Now binary search within the period to find the exact oldest timestamp
    let search_end = now.saturating_sub(last_year_with_events.saturating_sub(1) * YEAR_SECONDS);
    let search_start = if found_empty_period {
        now.saturating_sub(years_back * YEAR_SECONDS)
    } else {
        0 // If we went back 10 years and still found events, search from beginning of time
    };

    let mut left = search_start;
    let mut right = search_end;
    let mut oldest_found = right;

    info!("Binary searching between {} and {}", left, right);

    while left < right {
        let mid = left + (right - left) / 2;

        // Check if there are any events before mid timestamp
        let filter = Filter::new()
            .kind(Kind::Metadata)
            .until(Timestamp::from(mid))
            .limit(1);

        match database.query(vec![filter], &scope).await {
            Ok(events) => {
                if let Some(event) = events.into_iter().next() {
                    // Found an event, so there might be older ones
                    oldest_found = event.created_at.as_u64();
                    right = mid;
                } else {
                    // No events before mid, search in the right half
                    left = mid + 1;
                }
            }
            Err(e) => {
                error!("Error during binary search: {}", e);
                return Err(e.into());
            }
        }

        // Precision threshold - stop when we're within a day
        if right - left < 86400 {
            break;
        }
    }

    // Do one final query to get the actual oldest event
    let final_filter = Filter::new()
        .kind(Kind::Metadata)
        .until(Timestamp::from(oldest_found + 86400)) // Add a day buffer
        .limit(1);

    if let Ok(events) = database.query(vec![final_filter], &scope).await {
        if let Some(event) = events.into_iter().next() {
            oldest_found = event.created_at.as_u64();
        }
    }

    let span_days = (now - oldest_found) / 86400;
    info!(
        "Oldest timestamp found: {} ({} days ago)",
        oldest_found, span_days
    );

    Ok(oldest_found)
}

/// Update the cached profile count
async fn update_profile_count(
    database: &Arc<RelayDatabase>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let scope = nostr_lmdb::Scope::Default;
    let filter = Filter::new().kind(Kind::Metadata);
    let count = database.count(vec![filter], &scope).await?;
    Ok(count)
}

async fn random_profiles_handler_impl(
    database: Arc<RelayDatabase>,
    params: RandomQuery,
    headers: HeaderMap,
) -> Response {
    // Cap the count at 500
    let count = params.count.min(500);
    info!("Random profiles requested: count={}", count);

    // Get cached oldest timestamp
    let oldest_timestamp = match OLDEST_TIMESTAMP.read().unwrap().as_ref() {
        Some(&ts) => ts,
        None => {
            info!("Oldest timestamp not initialized");
            return Json::<Vec<ProfileWithRelays>>(vec![]).into_response();
        }
    };

    use rand::Rng;
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::from_entropy();

    let scope = nostr_lmdb::Scope::Default;
    let mut selected_profiles = Vec::new();
    let mut attempts = 0;
    let max_attempts = count * 3; // Allow more attempts for larger counts

    let now = Timestamp::now().as_u64();

    // Collect random profiles one by one
    while selected_profiles.len() < count && attempts < max_attempts {
        attempts += 1;

        // Pick a random timestamp between oldest and now
        let random_timestamp = rng.gen_range(oldest_timestamp..=now);

        // Query for the most recent profile before this timestamp
        let filter = Filter::new()
            .kind(Kind::Metadata)
            .until(Timestamp::from(random_timestamp))
            .limit(1);

        match database.query(vec![filter], &scope).await {
            Ok(events) => {
                if let Some(event) = events.into_iter().next() {
                    // Check if we already have this profile (avoid duplicates)
                    if !selected_profiles
                        .iter()
                        .any(|e: &Event| e.pubkey == event.pubkey)
                    {
                        selected_profiles.push(event);
                    }
                }
            }
            Err(e) => {
                error!("Error querying random profile: {}", e);
            }
        }
    }

    info!(
        "Selected {} random profiles after {} attempts",
        selected_profiles.len(),
        attempts
    );

    // Now fetch relay lists for each profile
    let mut results = Vec::new();
    for profile in selected_profiles {
        let relay_filter = Filter::new()
            .author(profile.pubkey)
            .kinds(vec![Kind::Custom(10002), Kind::Custom(10050)]);

        let mut relay_list = None;
        let mut dm_relay_list = None;

        let scope = nostr_lmdb::Scope::Default;
        if let Ok(relay_events) = database.query(vec![relay_filter], &scope).await {
            for event in relay_events {
                match event.kind.as_u16() {
                    10002 => relay_list = Some(event),
                    10050 => dm_relay_list = Some(event),
                    _ => {}
                }
            }
        }

        results.push(ProfileWithRelays {
            profile,
            relay_list,
            dm_relay_list,
        });
    }

    // Sort results by created_at in descending order (most recent first)
    results.sort_by(|a, b| b.profile.created_at.cmp(&a.profile.created_at));

    // Check Accept header to determine response type
    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if accept.contains("text/html") {
        // Return HTML response
        Html(render_random_profiles_html(&results)).into_response()
    } else {
        // Return JSON response
        Json(results).into_response()
    }
}

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
    let task_tracker = TaskTracker::new();
    let crypto_sender = CryptoWorker::spawn(Arc::new(keys.clone()), &task_tracker);
    let database_path = std::env::var("DATABASE_PATH")
        .unwrap_or_else(|_| "./data/profile_aggregator.db".to_string());

    let database = Arc::new(RelayDatabase::new(&database_path, crypto_sender)?);

    // Find and cache the oldest timestamp for random selection
    match find_oldest_timestamp(&database).await {
        Ok(oldest) => {
            *OLDEST_TIMESTAMP.write().unwrap() = Some(oldest);
        }
        Err(e) => {
            error!("Failed to find oldest timestamp: {}", e);
            // Continue anyway - random endpoint will return empty results
        }
    }

    // Initialize and periodically update profile count
    let database_clone = database.clone();
    tokio::spawn(async move {
        loop {
            match update_profile_count(&database_clone).await {
                Ok(count) => {
                    *PROFILE_COUNT.write().unwrap() = count;
                    info!("Updated profile count: {} profiles", count);
                }
                Err(e) => {
                    error!("Failed to update profile count: {}", e);
                }
            }
            // Wait 5 minutes before next update
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

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
        contact: std::env::var("RELAY_CONTACT").unwrap_or_else(|_| "daniel@nos.social".to_string()),
        supported_nips: vec![1, 11],
        software: "profile_aggregator".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        icon: None,
    };

    // Build the WebSocket server without default HTML
    let ws_handler = RelayBuilder::new(config.clone())
        .without_html()
        .build_axum_handler(filter.as_ref().clone(), relay_info.clone())
        .await?;

    // Create custom root handler that combines WebSocket and HTML
    let root_handler = {
        let relay_info = relay_info.clone();
        let relay_url = config.relay_url.clone();
        let ws_handler = ws_handler.clone();
        move |ws: Option<axum::extract::WebSocketUpgrade>,
              ConnectInfo(addr): ConnectInfo<SocketAddr>,
              headers: HeaderMap| {
            let info = relay_info.clone();
            let url = relay_url.clone();
            let ws_h = ws_handler.clone();
            async move {
                // If it's a WebSocket upgrade, delegate to the WebSocket handler
                if let Some(ws) = ws {
                    return ws_h(Some(ws), ConnectInfo(addr), headers).await;
                }

                // Check if client wants JSON (NIP-11)
                if let Some(accept) = headers.get("accept") {
                    if let Ok(accept_str) = accept.to_str() {
                        if accept_str.contains("application/nostr+json") {
                            return Json(&info).into_response();
                        }
                    }
                }

                // Otherwise return HTML
                let websocket_url = url.replace("ws://", "wss://");
                let html = render_index_html(&info, &websocket_url);

                Html(html).into_response()
            }
        }
    };

    // Create HTTP server
    // Clone database for the handler
    let db_for_handler = database.clone();

    // Create the handler as a closure
    let handler = move |Query(params): Query<RandomQuery>, headers: HeaderMap| {
        let db = db_for_handler.clone();
        async move { random_profiles_handler_impl(db, params, headers).await }
    };

    // Create router with both WebSocket and HTML handlers
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(|| async { "OK" }))
        .route("/random", get(handler));

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

    // No need to periodically refresh - oldest timestamp is stable

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
