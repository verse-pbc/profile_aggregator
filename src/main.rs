use anyhow::Result;
use axum::{
    extract::{ConnectInfo, Query},
    http::HeaderMap,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use heavykeeper::TopK;
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

#[derive(Serialize, Clone)]
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
// Global cache for random profiles
static RANDOM_PROFILES_CACHE: RwLock<Vec<ProfileWithRelays>> = RwLock::new(Vec::new());

// Global TopK tracker for shown profiles
lazy_static::lazy_static! {
    static ref SHOWN_PROFILES: RwLock<TopK<Vec<u8>>> = RwLock::new(
        // Track top 1000 most shown profiles
        // width=5000, depth=4 for good accuracy
        // decay=0.9 for gradual forgetting
        TopK::new(1000, 5000, 4, 0.9)
    );
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

/// Refresh the random profiles cache with up to 10,000 items using window-based queries
async fn refresh_random_profiles_cache(database: Arc<RelayDatabase>) -> Result<()> {
    info!("Refreshing random profiles cache...");

    const TARGET_COUNT: usize = 10_000;
    let scope = nostr_lmdb::Scope::Default;
    let now = Timestamp::now();

    // Get the current oldest timestamp (if we have one)
    let cached_oldest = *OLDEST_TIMESTAMP.read().unwrap();

    // Decide on the window to query
    let (until_timestamp, is_initial) = if cached_oldest.is_none() {
        // First time - start with the most recent window
        (now, true)
    } else {
        // For refresh, pick a random window
        use rand::Rng;
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::from_entropy();
        let oldest = cached_oldest.unwrap();
        let random_ts = rng.gen_range(oldest..=now.as_u64());
        (Timestamp::from(random_ts), false)
    };

    // Query a window of up to 10,000 profiles
    let filter = Filter::new()
        .kind(Kind::Metadata)
        .until(until_timestamp)
        .limit(TARGET_COUNT);

    let mut profiles = match database.query(vec![filter], &scope).await {
        Ok(events) => events.into_iter().collect::<Vec<_>>(),
        Err(e) => {
            error!("Error querying profiles for cache: {}", e);
            return Ok(());
        }
    };

    info!(
        "Fetched {} profiles from window until={}",
        profiles.len(),
        until_timestamp.as_u64()
    );

    // If this is the initial load and we got exactly 10,000, fetch one more window to expand our range
    if is_initial && profiles.len() == TARGET_COUNT {
        // Get the oldest from current batch
        let current_oldest = profiles
            .last()
            .map(|p| p.created_at.as_u64())
            .unwrap_or(now.as_u64());

        // Fetch ONE more window to expand our range
        let filter = Filter::new()
            .kind(Kind::Metadata)
            .until(Timestamp::from(current_oldest))
            .limit(TARGET_COUNT);

        match database.query(vec![filter], &scope).await {
            Ok(events) => {
                let older_batch: Vec<_> = events.into_iter().collect();
                if !older_batch.is_empty() {
                    // Update oldest timestamp based on this batch
                    if let Some(oldest_event) = older_batch.last() {
                        let new_oldest = oldest_event.created_at.as_u64();
                        *OLDEST_TIMESTAMP.write().unwrap() = Some(new_oldest);
                        info!("Expanded range: found older timestamp {} from second window with {} profiles", 
                              new_oldest, older_batch.len());
                    }
                    // Add the older profiles to our collection
                    profiles.extend(older_batch);
                    info!("Total profiles after combining windows: {}", profiles.len());
                }
            }
            Err(e) => {
                error!("Error fetching second window: {}", e);
            }
        }
    } else if profiles.len() < TARGET_COUNT && cached_oldest.is_some() {
        // We got fewer than expected, maybe we need to supplement from the most recent window
        let recent_filter = Filter::new()
            .kind(Kind::Metadata)
            .until(now)
            .limit(TARGET_COUNT - profiles.len());

        if let Ok(events) = database.query(vec![recent_filter], &scope).await {
            let recent_profiles: Vec<_> = events
                .into_iter()
                .filter(|e| !profiles.iter().any(|p| p.pubkey == e.pubkey))
                .collect();
            let supplement_count = recent_profiles.len();
            profiles.extend(recent_profiles);
            info!("Supplemented with {} recent profiles", supplement_count);
        }
    }

    // Update oldest timestamp if we found an older one
    if let Some(oldest_in_batch) = profiles.last() {
        let batch_oldest = oldest_in_batch.created_at.as_u64();
        let mut oldest = OLDEST_TIMESTAMP.write().unwrap();
        if oldest.is_none() || batch_oldest < oldest.unwrap() {
            *oldest = Some(batch_oldest);
            info!("Updated oldest timestamp to: {}", batch_oldest);
        }
    }

    // Remove duplicates based on pubkey
    let mut seen_pubkeys = std::collections::HashSet::new();
    profiles.retain(|p| seen_pubkeys.insert(p.pubkey));

    info!(
        "Fetching relay lists for {} unique profiles",
        profiles.len()
    );

    // Batch fetch relay lists - fetch all at once instead of one by one
    let authors: Vec<_> = profiles.iter().map(|p| p.pubkey).collect();
    let relay_filter = Filter::new()
        .authors(authors)
        .kinds(vec![Kind::Custom(10002), Kind::Custom(10050)]);

    let mut relay_events_map: std::collections::HashMap<PublicKey, (Option<Event>, Option<Event>)> =
        std::collections::HashMap::new();

    if let Ok(relay_events) = database.query(vec![relay_filter], &scope).await {
        for event in relay_events {
            let entry = relay_events_map.entry(event.pubkey).or_insert((None, None));
            match event.kind.as_u16() {
                10002 => entry.0 = Some(event),
                10050 => entry.1 = Some(event),
                _ => {}
            }
        }
    }

    // Build results with relay lists
    let mut results = Vec::new();
    for profile in profiles {
        let (relay_list, dm_relay_list) = relay_events_map
            .get(&profile.pubkey)
            .cloned()
            .unwrap_or((None, None));

        results.push(ProfileWithRelays {
            profile,
            relay_list,
            dm_relay_list,
        });
    }

    // Sort by created_at in descending order
    results.sort_by(|a, b| b.profile.created_at.cmp(&a.profile.created_at));

    // Update the cache
    {
        let mut cache = RANDOM_PROFILES_CACHE.write().unwrap();
        *cache = results;
    }

    info!(
        "Random profiles cache refreshed with {} profiles",
        RANDOM_PROFILES_CACHE.read().unwrap().len()
    );

    Ok(())
}

async fn random_profiles_handler_impl(
    _database: Arc<RelayDatabase>,
    params: RandomQuery,
    headers: HeaderMap,
) -> Response {
    // Cap the count at 500
    let count = params.count.min(500);
    info!("Random profiles requested: count={}", count);

    // Get profiles from cache
    let cache = RANDOM_PROFILES_CACHE.read().unwrap();

    if cache.is_empty() {
        info!("Cache is empty, returning empty response");
        return Json::<Vec<ProfileWithRelays>>(vec![]).into_response();
    }

    // Get the set of frequently shown profiles
    let shown_tracker = SHOWN_PROFILES.read().unwrap();
    let frequently_shown: std::collections::HashSet<Vec<u8>> = shown_tracker
        .list()
        .into_iter()
        .map(|node| node.item)
        .collect();
    drop(shown_tracker);

    // Build indices for rarely shown and frequently shown profiles
    let mut rarely_shown_indices = Vec::new();
    let mut frequently_shown_indices = Vec::new();

    for (idx, profile) in cache.iter().enumerate() {
        if frequently_shown.contains(profile.profile.pubkey.to_bytes().as_slice()) {
            frequently_shown_indices.push(idx);
        } else {
            rarely_shown_indices.push(idx);
        }
    }

    info!(
        "Cache partitioned: {} rarely shown, {} frequently shown",
        rarely_shown_indices.len(),
        frequently_shown_indices.len()
    );

    // Randomly sample from the cache with priority to rarely shown
    use rand::seq::SliceRandom;
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::from_entropy();

    let mut results = Vec::new();

    // First, take from rarely shown profiles
    let rarely_shown_sample_indices: Vec<_> = rarely_shown_indices
        .choose_multiple(&mut rng, count.min(rarely_shown_indices.len()))
        .copied()
        .collect();

    for idx in rarely_shown_sample_indices {
        results.push(cache[idx].clone());
    }

    // If we need more, take from frequently shown
    if results.len() < count && !frequently_shown_indices.is_empty() {
        let remaining = count - results.len();
        let frequently_shown_sample_indices: Vec<_> = frequently_shown_indices
            .choose_multiple(&mut rng, remaining.min(frequently_shown_indices.len()))
            .copied()
            .collect();

        for idx in frequently_shown_sample_indices {
            results.push(cache[idx].clone());
        }
    }

    // Track what we're showing
    {
        let mut shown_tracker = SHOWN_PROFILES.write().unwrap();
        for profile in &results {
            shown_tracker.add(profile.profile.pubkey.to_bytes().to_vec());
        }
    }

    // Sort results by created_at in descending order (most recent first)
    results.sort_by(|a, b| b.profile.created_at.cmp(&a.profile.created_at));

    info!("Returning {} profiles from cache", results.len());

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

    // The oldest timestamp will be discovered during the first cache refresh

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
            // Wait 1 minute before next update
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    // Initialize random profiles cache
    let database_clone = database.clone();
    info!("Initializing random profiles cache...");
    if let Err(e) = refresh_random_profiles_cache(database_clone.clone()).await {
        error!("Failed to initialize random profiles cache: {}", e);
    }

    // Periodically refresh the random profiles cache
    let database_clone = database.clone();
    tokio::spawn(async move {
        // Wait 1 minute before first refresh (since we just initialized)
        tokio::time::sleep(Duration::from_secs(60)).await;

        loop {
            if let Err(e) = refresh_random_profiles_cache(database_clone.clone()).await {
                error!("Failed to refresh random profiles cache: {}", e);
            }
            // Wait 1 minute before next refresh
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    // Periodically log the most shown profiles
    tokio::spawn(async move {
        loop {
            // Wait 5 minutes
            tokio::time::sleep(Duration::from_secs(300)).await;

            let shown_tracker = SHOWN_PROFILES.read().unwrap();
            let top_profiles = shown_tracker.list();
            if !top_profiles.is_empty() {
                info!("Top 10 most shown profiles:");
                for (i, node) in top_profiles.iter().take(10).enumerate() {
                    if let Ok(pubkey_bytes) = <[u8; 32]>::try_from(node.item.as_slice()) {
                        let pubkey = PublicKey::from_byte_array(pubkey_bytes);
                        info!("  {}. {} - shown {} times", i + 1, pubkey, node.count);
                    }
                }
            }
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
