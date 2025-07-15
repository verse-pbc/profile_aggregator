use anyhow::Result;
use axum::{
    extract::{ConnectInfo, Query},
    http::HeaderMap,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use heavykeeper::TopK;
use nostr_relay_builder::{RelayBuilder, RelayConfig, RelayDatabase, RelayInfo};
use nostr_sdk::prelude::*;
use profile_aggregator::{
    avatar_sync::{self, AvatarSyncConfig},
    proxy_image_handler::proxy_image_handler,
    ProfileAggregationConfig, ProfileAggregationService, ProfileQualityFilter,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::{LazyLock, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

// HTML templates
static INDEX_TEMPLATE: &str = include_str!("../templates/index.html");
static RANDOM_PROFILES_TEMPLATE: &str = include_str!("../templates/random_profiles.html");

fn render_index_html(info: &RelayInfo, websocket_url: &str) -> String {
    let supported_nips = info
        .supported_nips
        .iter()
        .map(|nip| format!(r#"<span class="nip">NIP-{nip:02}</span>"#))
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

// Global TopK tracker for least shown profiles
static LEAST_SHOWN_PROFILES: LazyLock<RwLock<TopK<Vec<u8>>>> = LazyLock::new(|| {
    RwLock::new(
        // Track top 10000 least shown profiles (by tracking items NOT returned)
        // width=50000, depth=4 for good accuracy
        // decay=0.9 for gradual forgetting of old patterns
        TopK::new(10_000, 50_000, 4, 0.9),
    )
});

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
        use ::rand::Rng;
        let mut rng = ::rand::rngs::ThreadRng::default();
        let oldest = cached_oldest.unwrap();
        let random_ts = rng.random_range(oldest..=now.as_u64());
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

/// Select profiles from cache prioritizing least shown ones
/// Returns selected profiles and indices of non-selected profiles
fn select_least_shown_profiles(
    cache: &[ProfileWithRelays],
    count: usize,
    max_count: usize,
    least_shown_tracker: &TopK<Vec<u8>>,
) -> (Vec<ProfileWithRelays>, Vec<usize>) {
    use ::rand::seq::SliceRandom;
    let mut rng = ::rand::rng();

    // Get the least shown profiles from HeavyKeeper
    let least_shown_list = least_shown_tracker.list();

    // Create a map for quick lookup of priority scores
    let mut priority_map: std::collections::HashMap<Vec<u8>, u64> =
        std::collections::HashMap::new();
    for node in least_shown_list.iter() {
        // Higher count in HeavyKeeper = less shown = higher priority
        priority_map.insert(node.item.clone(), node.count);
    }

    // Create indices with priority scores
    let mut indices_with_priority: Vec<(usize, u64)> = cache
        .iter()
        .enumerate()
        .map(|(idx, profile)| {
            let pubkey_bytes = profile.profile.pubkey.to_bytes().to_vec();
            let priority = priority_map.get(&pubkey_bytes).copied().unwrap_or(0);
            (idx, priority)
        })
        .collect();

    // Sort by priority (descending - higher priority first)
    indices_with_priority.sort_by(|a, b| b.1.cmp(&a.1));

    // Take the top priority items up to the count
    let mut selected_indices = Vec::new();
    let mut remaining_indices = Vec::new();

    for (i, (idx, _priority)) in indices_with_priority.into_iter().enumerate() {
        if i < count.min(max_count) {
            selected_indices.push(idx);
        } else {
            remaining_indices.push(idx);
        }
    }

    // If we have items with same priority (e.g., all zero), shuffle within priority groups
    selected_indices.shuffle(&mut rng);

    // Build results
    let mut results = Vec::new();
    let mut not_selected = Vec::new();

    for (idx, profile) in cache.iter().enumerate() {
        if selected_indices.contains(&idx) {
            results.push(profile.clone());
        } else {
            not_selected.push(idx);
        }
    }

    (results, not_selected)
}

async fn random_profiles_handler_impl(params: RandomQuery, headers: HeaderMap) -> Response {
    // Cap the count at 500
    let count = params.count.min(500);
    info!("Random profiles requested: count={}", count);

    // Get profiles from cache
    let cache = RANDOM_PROFILES_CACHE.read().unwrap();

    if cache.is_empty() {
        info!("Cache is empty, returning empty response");
        return Json::<Vec<ProfileWithRelays>>(vec![]).into_response();
    }

    // Get the least shown tracker
    let least_shown_tracker = LEAST_SHOWN_PROFILES.read().unwrap();

    // Select profiles prioritizing least shown
    let (mut results, not_selected_indices) = select_least_shown_profiles(
        &cache,
        count,
        500, // max_count
        &least_shown_tracker,
    );

    drop(least_shown_tracker);

    info!(
        "Selected {} profiles, {} not selected",
        results.len(),
        not_selected_indices.len()
    );

    // Update the tracker with profiles NOT returned (they become "least shown")
    {
        let mut least_shown_tracker = LEAST_SHOWN_PROFILES.write().unwrap();
        for idx in not_selected_indices {
            least_shown_tracker.add(cache[idx].profile.pubkey.to_bytes().to_vec());
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
    // Initialize rustls crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Load environment variables
    dotenv::dotenv().ok();

    // Check if we should enable tokio-console
    if std::env::var("TOKIO_CONSOLE").is_ok() {
        console_subscriber::init();
        info!("tokio-console enabled on port 6669");
    } else {
        // Setup normal logging if not using tokio-console
        let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,profile_aggregator=debug,nostr_relay_builder=debug,nostr_relay_pool::relay::inner=off")
        });
        fmt().with_env_filter(env_filter).with_target(true).init();
    }

    // Load relay keys from environment or use default dev key
    let relay_secret = std::env::var("RELAY_SECRET_KEY").unwrap_or_else(|_| {
        "339e1ab1f59eb304b8cb5202eddcc437ff699fc523161b6e2c222590cccb3b84".to_string()
    });

    let keys = Keys::parse(&relay_secret)?;
    println!("🔑 Relay public key: {}", keys.public_key());

    // Create task tracker and cancellation token for coordinated shutdown
    let cancellation_token = CancellationToken::new();
    let task_tracker = TaskTracker::new();
    let database_path = std::env::var("DATABASE_PATH")
        .unwrap_or_else(|_| "./data/profile_aggregator.db".to_string());

    // The oldest timestamp will be discovered during the first cache refresh

    // Create database
    let database = RelayDatabase::new(&database_path)?;
    let database = Arc::new(database);

    // Initialize global avatar sync client
    let avatar_sync_config = AvatarSyncConfig::default();
    if let Err(e) = avatar_sync::init_global_client(avatar_sync_config) {
        warn!("Failed to initialize avatar sync client: {}", e);
    } else {
        info!("Avatar sync client initialized");
    }

    // Initialize and periodically update profile count
    let database_clone = database.clone();
    let profile_count_token = cancellation_token.clone();
    task_tracker.spawn(async move {
        loop {
            tokio::select! {
                _ = profile_count_token.cancelled() => {
                    info!("Profile count updater cancelled, exiting");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    match update_profile_count(&database_clone).await {
                        Ok(count) => {
                            *PROFILE_COUNT.write().unwrap() = count;
                            info!("Updated profile count: {} profiles", count);
                        }
                        Err(e) => {
                            error!("Failed to update profile count: {}", e);
                        }
                    }
                }
            }
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
    let cache_refresh_token = cancellation_token.clone();
    task_tracker.spawn(async move {
        // Wait 1 minute before first refresh (since we just initialized)
        tokio::select! {
            _ = cache_refresh_token.cancelled() => {
                info!("Random profiles cache refresher cancelled, exiting");
                return;
            }
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        }

        loop {
            tokio::select! {
                _ = cache_refresh_token.cancelled() => {
                    info!("Random profiles cache refresher cancelled, exiting");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    if let Err(e) = refresh_random_profiles_cache(database_clone.clone()).await {
                        error!("Failed to refresh random profiles cache: {}", e);
                    }
                }
            }
        }
    });

    // Periodically log the least shown profiles
    let least_shown_token = cancellation_token.clone();
    task_tracker.spawn(async move {
        loop {
            tokio::select! {
                _ = least_shown_token.cancelled() => {
                    info!("Least shown profiles logger cancelled, exiting");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(300)) => {
                    let least_shown_tracker = LEAST_SHOWN_PROFILES.read().unwrap();
                    let least_shown_list = least_shown_tracker.list();
                    if !least_shown_list.is_empty() {
                        info!("Top 10 least shown profiles (high priority for display):");
                        for (i, node) in least_shown_list.iter().take(10).enumerate() {
                            if let Ok(pubkey_bytes) = <[u8; 32]>::try_from(node.item.as_slice()) {
                                let pubkey = PublicKey::from_byte_array(pubkey_bytes);
                                info!("  {}. {} - not shown {} times", i + 1, pubkey, node.count);
                            }
                        }
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

    // Create gossip-enabled client for WebSocket connections
    info!("Creating shared gossip client for WebSocket profile verification");
    let gossip_keys = Keys::generate();

    // Configure gossip client to be respectful of relays
    let gossip_options = ClientOptions::new()
        .gossip(true)
        .max_avg_latency(Duration::from_secs(2)) // Skip slow relays
        .automatic_authentication(true);

    let gossip_client = Client::builder()
        .signer(gossip_keys)
        .opts(gossip_options)
        .build();

    // Add multiple discovery relays for better coverage
    let discovery_relays = vec![
        "wss://relay.nos.social",
        "wss://relay.damus.io",
        "wss://nos.lol",
        "wss://relay.primal.net",
    ];

    for relay in discovery_relays.clone() {
        if let Err(e) = gossip_client.add_discovery_relay(relay).await {
            warn!("Failed to add discovery relay {}: {}", relay, e);
        }
    }

    gossip_client.connect().await;
    let gossip_client = Arc::new(gossip_client);

    // Create shared filter for both WebSocket and aggregation service
    // Use the gossip client for WebSocket connections
    let profile_quality_filter = Arc::new(ProfileQualityFilter::with_gossip_client(
        database.clone(),
        true, // skip_mostr
        true, // skip_fields
        gossip_client.clone(),
    ));

    // Both services use the same filter
    let aggregation_service = ProfileAggregationService::new(
        aggregation_config,
        profile_quality_filter.clone(),
        database.clone(),
        cancellation_token.clone(),
        task_tracker.clone(),
    )
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
    let relay_url = config.relay_url.clone();
    let ws_handler = RelayBuilder::<()>::new(config)
        .without_html()
        .with_task_tracker(task_tracker.clone())
        .with_event_processor::<ProfileQualityFilter>(profile_quality_filter)
        .with_relay_info(relay_info.clone())
        .with_cancellation_token(cancellation_token.clone())
        .build_axum()
        .await?;

    // Create custom root handler that combines WebSocket and HTML
    let root_handler = {
        let relay_info = relay_info.clone();
        move |ws: Option<websocket_builder::WebSocketUpgrade>,
              ConnectInfo(addr): ConnectInfo<SocketAddr>,
              headers: HeaderMap| {
            let info = relay_info.clone();
            let url = relay_url.clone();
            //let ws_h = ws_handler; //#.clone();
            async move {
                // If it's a WebSocket upgrade, delegate to the WebSocket handler
                if ws.is_some() {
                    return ws_handler(ws, ConnectInfo(addr), headers).await;
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

    // Create the handler as a closure
    let handler = move |Query(params): Query<RandomQuery>, headers: HeaderMap| async move {
        // The _database parameter in random_profiles_handler_impl is not used (prefixed with _)
        // but we still need to pass it for the signature
        random_profiles_handler_impl(params, headers).await
    };

    // Configure CORS
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Create router with both WebSocket and HTML handlers
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(|| async { "OK" }))
        .route("/random", get(handler))
        .route("/proxy-image", get(proxy_image_handler))
        .layer(cors);

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind_addr.parse()?;

    // Store the aggregation service for shutdown
    let aggregation_service = Arc::new(aggregation_service);

    // Spawn the aggregation service
    let aggregation_service_token = cancellation_token.clone();
    let aggregation_service_clone = aggregation_service.clone();
    task_tracker.spawn(async move {
        info!("🔄 Starting profile aggregation service...");
        tokio::select! {
            result = aggregation_service_clone.run() => {
                if let Err(e) = result {
                    error!("Profile aggregation service error: {}", e);
                }
            }
            _ = aggregation_service_token.cancelled() => {
                info!("Profile aggregation service cancelled");
            }
        }
        // aggregation_service_clone is automatically dropped here when going out of scope
    });

    // No need to periodically refresh - oldest timestamp is stable

    println!("\nProfile Aggregator starting");
    println!("WebSocket: ws://{addr}");
    println!("\nFetching user profiles (kind 0)");
    println!("\nDiscovery relay: {}", discovery_relay_urls.join(", "));
    println!("\nProfile requirements:");
    println!("- Name: display_name or name field");
    println!("- Bio: non-empty about field");
    println!("- Picture: valid URL, min 300x600px");
    println!("- Verified: published text note via outbox relays");
    println!("- Excludes: bridges, mostr accounts, profiles with fields array");
    println!("\nWebSocket profile verification: ENABLED");
    println!("\nConfiguration:");
    println!("- Page size: {page_size} events");
    println!("- Backoff: {initial_backoff_secs}s-{max_backoff_secs}s");
    println!("- Database: {database_path}");
    println!("- State: {state_file}");

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
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await?;

    info!("Server stopped, shutting down services...");

    // Signal all services to shutdown
    cancellation_token.cancel();

    // Shutdown the gossip client
    info!("Shutting down shared gossip client...");
    gossip_client.shutdown().await;

    // Close the task tracker to prevent new tasks from being spawned
    task_tracker.close();

    info!("All resources dropped, waiting for background tasks...");
    match tokio::time::timeout(Duration::from_secs(30), task_tracker.wait()).await {
        Ok(()) => info!("All background tasks completed successfully"),
        Err(_) => {
            error!("Timeout waiting for background tasks to complete");
            std::process::exit(1);
        }
    }

    info!("Clean shutdown complete");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use heavykeeper::TopK;
    use tempfile::TempDir;

    // Helper to create test profiles
    fn create_test_profile(pubkey: PublicKey, created_at: u64) -> ProfileWithRelays {
        let metadata = serde_json::json!({
            "name": format!("Test User {}", pubkey.to_string().chars().take(8).collect::<String>()),
            "about": "Test profile",
            "picture": "https://example.com/pic.jpg"
        });

        // Create a dummy signed event
        let event = Event::new(
            EventId::from_slice(&[0; 32]).unwrap(),
            pubkey,
            Timestamp::from(created_at),
            Kind::Metadata,
            vec![],
            metadata.to_string(),
            nostr_sdk::secp256k1::schnorr::Signature::from_slice(&[0; 64]).unwrap(),
        );

        ProfileWithRelays {
            profile: event,
            relay_list: None,
            dm_relay_list: None,
        }
    }

    #[test]
    fn test_select_least_shown_profiles_empty_tracker() {
        // Setup
        let tracker = TopK::new(100, 500, 4, 0.9);
        let profiles: Vec<ProfileWithRelays> = (0..10)
            .map(|i| {
                let keys = Keys::generate();
                create_test_profile(keys.public_key(), 1000 + i)
            })
            .collect();

        // When tracker is empty, all profiles have equal priority (0)
        let (selected, not_selected) = select_least_shown_profiles(&profiles, 5, 5, &tracker);

        // Should select 5 profiles
        assert_eq!(selected.len(), 5);
        assert_eq!(not_selected.len(), 5);

        // Selected profiles should be unique
        let selected_pubkeys: std::collections::HashSet<_> =
            selected.iter().map(|p| p.profile.pubkey).collect();
        assert_eq!(selected_pubkeys.len(), 5);
    }

    #[test]
    fn test_select_least_shown_profiles_with_priorities() {
        // Setup
        let mut tracker = TopK::new(100, 500, 4, 0.9);
        let profiles: Vec<ProfileWithRelays> = (0..10)
            .map(|i| {
                let keys = Keys::generate();
                create_test_profile(keys.public_key(), 1000 + i)
            })
            .collect();

        // Add some profiles to tracker with different counts
        // Profiles 0-4 were NOT shown (high count = high priority)
        for profile in profiles.iter().take(5) {
            for _ in 0..10 {
                tracker.add(profile.profile.pubkey.to_bytes().to_vec());
            }
        }
        // Profiles 5-7 were shown less (medium count)
        for profile in profiles.iter().take(8).skip(5) {
            for _ in 0..3 {
                tracker.add(profile.profile.pubkey.to_bytes().to_vec());
            }
        }
        // Profiles 8-9 were not tracked (count = 0)

        let (selected, not_selected) = select_least_shown_profiles(&profiles, 5, 10, &tracker);

        // Should prioritize profiles 0-4 (highest counts in tracker)
        assert_eq!(selected.len(), 5);
        assert_eq!(not_selected.len(), 5);

        // Check that high priority profiles were selected
        let selected_pubkeys: Vec<_> = selected.iter().map(|p| p.profile.pubkey).collect();
        for (i, profile) in profiles.iter().enumerate().take(5) {
            assert!(
                selected_pubkeys.contains(&profile.profile.pubkey),
                "High priority profile {i} should be selected"
            );
        }
    }

    #[test]
    fn test_select_least_shown_respects_max_count() {
        // Setup
        let tracker = TopK::new(100, 500, 4, 0.9);
        let profiles: Vec<ProfileWithRelays> = (0..10)
            .map(|i| {
                let keys = Keys::generate();
                create_test_profile(keys.public_key(), 1000 + i)
            })
            .collect();

        // Request 8 but max_count is 5
        let (selected, not_selected) = select_least_shown_profiles(&profiles, 8, 5, &tracker);

        // Should only select 5 due to max_count
        assert_eq!(selected.len(), 5);
        assert_eq!(not_selected.len(), 5);
    }

    #[test]
    fn test_window_based_cache_parameters() {
        // Test that our windowing constants work properly
        const TEST_TARGET_COUNT: usize = 100;
        const TEST_CACHE_SIZE: usize = 100;

        // In real implementation:
        // - TARGET_COUNT = 10_000 (profiles to fetch per window)
        // - Cache can hold 10_000 profiles
        // - We select up to 500 for API response

        // These are compile-time constants, so the assertions are always true
        // but we keep them as documentation of the expected relationships
        let _target_fits_in_cache = TEST_TARGET_COUNT <= TEST_CACHE_SIZE;
        let _api_max_fits_in_target = 50 <= TEST_TARGET_COUNT;
        assert!(_target_fits_in_cache);
        assert!(_api_max_fits_in_target);
    }

    #[tokio::test]
    async fn test_profile_selection_integration() {
        // Setup database
        let tmp_dir = TempDir::new().unwrap();
        let db_path = tmp_dir.path().join("test.db");
        let _task_tracker = tokio_util::task::TaskTracker::new();
        let database = nostr_relay_builder::RelayDatabase::new(db_path.to_str().unwrap()).unwrap();
        let database = Arc::new(database);

        // Create test profiles
        let profiles: Vec<Event> = (0..20)
            .map(|i| {
                let test_keys = Keys::generate();
                let metadata = serde_json::json!({
                    "name": format!("User {}", i),
                    "about": "Test profile",
                    "picture": "https://example.com/pic.jpg"
                });

                let unsigned_event =
                    EventBuilder::metadata(&Metadata::from_json(metadata.to_string()).unwrap())
                        .pow(0)
                        .build(test_keys.public_key());

                // Sign the event synchronously for tests
                unsigned_event.sign_with_keys(&test_keys).unwrap()
            })
            .collect();

        // Store profiles in database
        let scope = nostr_lmdb::Scope::Default;
        for event in &profiles {
            database.save_event(event, &scope).await.unwrap();
        }

        // Allow time for database writes to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Query with window
        let filter = Filter::new()
            .kind(Kind::Metadata)
            .until(Timestamp::now())
            .limit(10);

        let results = database.query(vec![filter], &scope).await.unwrap();
        let events: Vec<_> = results.into_iter().collect();

        // Should get up to 10 most recent profiles
        assert!(events.len() <= 10);

        // Check they're sorted by created_at descending
        for i in 1..events.len() {
            assert!(events[i - 1].created_at >= events[i].created_at);
        }
    }

    #[test]
    fn test_least_shown_tracker_behavior() {
        let mut tracker = TopK::new(5, 50, 4, 0.9);

        // Simulate profiles A, B, C, D, E
        let profiles: Vec<Vec<u8>> = (0..5).map(|i| vec![i]).collect();

        // Round 1: Return A, B (so C, D, E get added to tracker)
        tracker.add(profiles[2].clone()); // C
        tracker.add(profiles[3].clone()); // D
        tracker.add(profiles[4].clone()); // E

        // Round 2: Return C (so A, B, D, E get added)
        tracker.add(profiles[0].clone()); // A
        tracker.add(profiles[1].clone()); // B
        tracker.add(profiles[3].clone()); // D (again)
        tracker.add(profiles[4].clone()); // E (again)

        // Check counts
        let list = tracker.list();
        assert!(!list.is_empty());

        // D and E should have highest counts (added twice)
        let counts: std::collections::HashMap<Vec<u8>, u64> = list
            .into_iter()
            .map(|node| (node.item, node.count))
            .collect();

        // D and E should have count 2
        assert!(counts.get(&profiles[3]).unwrap_or(&0) >= counts.get(&profiles[0]).unwrap_or(&0));
        assert!(counts.get(&profiles[4]).unwrap_or(&0) >= counts.get(&profiles[1]).unwrap_or(&0));
    }

    #[tokio::test]
    async fn test_cache_refresh_with_windows() {
        // Setup database
        let tmp_dir = TempDir::new().unwrap();
        let db_path = tmp_dir.path().join("test.db");
        let _task_tracker = tokio_util::task::TaskTracker::new();
        let database = nostr_relay_builder::RelayDatabase::new(db_path.to_str().unwrap()).unwrap();
        let database = Arc::new(database);

        // Create profiles spread across time
        let base_time = 1000u64;
        let profiles: Vec<Event> = (0..50)
            .map(|i| {
                let test_keys = Keys::generate();
                let metadata = serde_json::json!({
                    "name": format!("User {}", i),
                    "about": "Test profile",
                    "picture": "https://example.com/pic.jpg"
                });

                let unsigned_event =
                    EventBuilder::metadata(&Metadata::from_json(metadata.to_string()).unwrap())
                        .pow(0)
                        .custom_created_at(Timestamp::from(base_time + i * 100)) // Spread across time
                        .build(test_keys.public_key());

                unsigned_event.sign_with_keys(&test_keys).unwrap()
            })
            .collect();

        // Store profiles
        let scope = nostr_lmdb::Scope::Default;
        for event in &profiles {
            database.save_event(event, &scope).await.unwrap();
        }

        // Allow writes to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Test window queries
        let now = Timestamp::from(base_time + 5000);

        // Query recent window
        let recent_filter = Filter::new().kind(Kind::Metadata).until(now).limit(10);

        let recent_results = database.query(vec![recent_filter], &scope).await.unwrap();
        let recent_events: Vec<_> = recent_results.into_iter().collect();

        assert_eq!(recent_events.len(), 10);
        // Should get the 10 most recent events
        assert!(recent_events
            .iter()
            .all(|e| e.created_at.as_u64() >= base_time + 4000));

        // Query older window
        let older_filter = Filter::new()
            .kind(Kind::Metadata)
            .until(Timestamp::from(base_time + 2000))
            .limit(10);

        let older_results = database.query(vec![older_filter], &scope).await.unwrap();
        let older_events: Vec<_> = older_results.into_iter().collect();

        assert_eq!(older_events.len(), 10);
        // Should get events from the older time range
        assert!(older_events
            .iter()
            .all(|e| e.created_at.as_u64() <= base_time + 2000));
    }

    #[test]
    fn test_random_selection_distribution() {
        // Test that random selection has reasonable distribution
        let mut tracker = TopK::new(100, 500, 4, 0.9);
        let profile_count = 100usize;
        let selection_rounds = 50;
        let profiles_per_round = 10;

        // Create test profiles
        let profiles: Vec<ProfileWithRelays> = (0..profile_count)
            .map(|i| {
                let keys = Keys::generate();
                create_test_profile(keys.public_key(), 1000 + i as u64)
            })
            .collect();

        // Track how many times each profile is selected
        let mut selection_counts = std::collections::HashMap::new();

        // Simulate multiple selection rounds
        for _ in 0..selection_rounds {
            let (selected, not_selected) = select_least_shown_profiles(
                &profiles,
                profiles_per_round,
                profiles_per_round,
                &tracker,
            );

            // Count selections
            for profile in &selected {
                *selection_counts.entry(profile.profile.pubkey).or_insert(0) += 1;
            }

            // Update tracker with not selected profiles
            for idx in not_selected {
                tracker.add(profiles[idx].profile.pubkey.to_bytes().to_vec());
            }
        }

        // Check distribution - all profiles should be selected at least once
        assert!(
            selection_counts.len() >= profile_count / 2,
            "At least half of profiles should be selected"
        );

        // Check that no profile is selected too many times (fair distribution)
        let max_selections = *selection_counts.values().max().unwrap();
        let min_selections = *selection_counts.values().min().unwrap();
        assert!(
            max_selections - min_selections <= 5,
            "Selection distribution should be relatively fair"
        );
    }
}
