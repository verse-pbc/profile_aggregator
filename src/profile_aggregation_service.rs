//! Profile Aggregation Service - Aggregates high-quality user profiles from external relays
//!
//! This service fetches user profiles from external relays, validates their quality,
//! and saves accepted profiles to the database for broadcasting.

use crate::profile_quality_filter::ProfileQualityFilter;
use crate::profile_validation_pool::ProfileValidationPool;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use nostr_relay_builder::RelayDatabase;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::{interval, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Configuration for the profile aggregation service
#[derive(Debug, Clone)]
pub struct ProfileAggregationConfig {
    /// URLs of relays to aggregate profiles from
    pub relay_urls: Vec<String>,
    /// Filters to apply when fetching events
    pub filters: Vec<Filter>,
    /// Number of events per page
    pub page_size: usize,
    /// Path to state persistence file
    pub state_file: PathBuf,
    /// Initial backoff duration for reconnection
    pub initial_backoff: Duration,
    /// Maximum backoff duration
    pub max_backoff: Duration,
    /// Number of worker threads for event processing
    pub worker_threads: usize,
}

impl Default for ProfileAggregationConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec![],
            filters: vec![Filter::new()],
            page_size: 500,
            state_file: PathBuf::from("./data/aggregation_state.json"),
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(300),
            worker_threads: std::thread::available_parallelism()
                .map(|n| n.get().min(10))
                .unwrap_or(5),
        }
    }
}

/// Persisted state for the aggregation service
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AggregationState {
    /// Map of relay URL to pagination info
    relay_states: HashMap<String, RelayPaginationInfo>,
    /// Last time the state was saved
    last_saved: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayPaginationInfo {
    /// Start of current window
    window_start: Timestamp,
    /// Last until timestamp used (for backward pagination)
    last_until_timestamp: Timestamp,
    /// Total events fetched from this relay
    total_events_fetched: u64,
    /// Whether we've completed the current window
    window_complete: bool,
}

/// Service that aggregates high-quality profiles from external relays
pub struct ProfileAggregationService {
    config: ProfileAggregationConfig,
    validation_pool: Arc<ProfileValidationPool>,
    state: Arc<RwLock<AggregationState>>,
    cancellation_token: CancellationToken,
}

impl ProfileAggregationService {
    /// Create a new profile aggregation service with a quality filter
    pub async fn new(
        config: ProfileAggregationConfig,
        filter: Arc<ProfileQualityFilter>,
        database: Arc<RelayDatabase>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Load or create state
        let state = match Self::load_state(&config.state_file) {
            Ok(state) => Arc::new(RwLock::new(state)),
            Err(_) => {
                info!("No previous aggregation state found, starting fresh");
                Arc::new(RwLock::new(AggregationState {
                    relay_states: HashMap::new(),
                    last_saved: Timestamp::now(),
                }))
            }
        };

        let cancellation_token = CancellationToken::new();

        // Create gossip-enabled client for outbox verification
        info!("Creating gossip client for outbox verification");
        let gossip_keys = Keys::generate();

        // Configure gossip client to be respectful of relays
        let gossip_options = Options::new()
            .gossip(true)
            .max_avg_latency(Duration::from_secs(2)) // Skip slow relays
            .automatic_authentication(true);

        let gossip_client = Client::builder()
            .signer(gossip_keys)
            .opts(gossip_options)
            .build();

        // Add discovery relay
        gossip_client
            .add_discovery_relay("wss://relay.nos.social")
            .await?;
        gossip_client.connect().await;

        let gossip_client = Arc::new(gossip_client);

        // Create profile validation pool with gossip client
        let validation_pool = Arc::new(ProfileValidationPool::new(
            config.worker_threads,
            filter.clone(),
            database.clone(),
            cancellation_token.clone(),
            gossip_client.clone(),
        ));

        Ok(Self {
            config,
            validation_pool,
            state,
            cancellation_token,
        })
    }

    /// Run the aggregation service
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!(
            "🚀 Starting profile aggregation service for {} relays with {} worker threads",
            self.config.relay_urls.len(),
            self.config.worker_threads,
        );

        // Create tasks for each relay
        let mut relay_handles = Vec::new();

        for relay_url in self.config.relay_urls.clone() {
            let config = self.config.clone();
            let validation_pool = self.validation_pool.clone();
            let state = self.state.clone();
            let cancellation_token = self.cancellation_token.clone();

            let handle = tokio::spawn(async move {
                let harvester = ProfileHarvester {
                    relay_url: relay_url.clone(),
                    config,
                    validation_pool,
                    state,
                    cancellation_token,
                };

                if let Err(e) = harvester.run().await {
                    error!("Profile harvester for {} failed: {}", relay_url, e);
                }
            });

            relay_handles.push(handle);
        }

        // Log metrics periodically
        let mut metrics_interval = interval(Duration::from_secs(60));
        let validation_pool = self.validation_pool.clone();

        // Periodic state saving
        let mut save_interval = interval(Duration::from_secs(30));
        let state_clone = self.state.clone();
        let state_file = self.config.state_file.clone();

        loop {
            tokio::select! {
                _ = self.cancellation_token.cancelled() => {
                    info!("Profile aggregation service cancelled");
                    break;
                }

                _ = save_interval.tick() => {
                    let state_snapshot = state_clone.read().await;
                    if let Err(e) = Self::save_state(&state_snapshot, &state_file).await {
                        error!("Failed to save aggregation state: {}", e);
                    }
                }

                _ = metrics_interval.tick() => {
                    let metrics = validation_pool.metrics();
                    info!(
                        "Profile validation metrics: queued={}, processed={}, accepted={}, rejected={}, failed={}",
                        metrics.queued_operations,
                        metrics.processed_profiles,
                        metrics.accepted_profiles,
                        metrics.rejected_profiles,
                        metrics.failed_operations
                    );
                }
            }
        }

        // Final save
        let state_snapshot = self.state.read().await;
        let _ = Self::save_state(&state_snapshot, &self.config.state_file).await;

        // Wait for all tasks
        for handle in relay_handles {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Save state to disk
    async fn save_state(
        state: &AggregationState,
        path: &PathBuf,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string_pretty(state)?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(path, json).await?;
        Ok(())
    }

    /// Load state from disk
    fn load_state(path: &PathBuf) -> Result<AggregationState, Box<dyn std::error::Error>> {
        let json = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&json)?)
    }
}

/// Worker that harvests profiles from a single relay
struct ProfileHarvester {
    relay_url: String,
    config: ProfileAggregationConfig,
    validation_pool: Arc<ProfileValidationPool>,
    state: Arc<RwLock<AggregationState>>,
    cancellation_token: CancellationToken,
}

impl ProfileHarvester {
    async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut backoff = self.config.initial_backoff;

        loop {
            if self.cancellation_token.is_cancelled() {
                break;
            }

            match self.connect_and_harvest().await {
                Ok(()) => {
                    backoff = self.config.initial_backoff;
                }
                Err(e) => {
                    error!("Profile aggregation failed for {}: {}", self.relay_url, e);
                }
            }

            sleep(backoff).await;
            backoff = std::cmp::min(backoff * 2, self.config.max_backoff);
        }

        Ok(())
    }

    async fn connect_and_harvest(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Connecting to {}", self.relay_url);

        // Initialize rustls if needed
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| "Failed to install rustls CryptoProvider")?;
        }

        let keys = Keys::generate();
        let client = Client::new(keys);
        client.add_relay(&self.relay_url).await?;
        client.connect().await;

        sleep(Duration::from_secs(2)).await;

        // Run backward pagination
        self.run_backward_pagination(client).await
    }

    async fn run_backward_pagination(
        &self,
        client: Client,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Get or initialize pagination state
        let (window_start, mut last_until) = {
            let state_guard = self.state.read().await;
            if let Some(info) = state_guard.relay_states.get(&self.relay_url) {
                if info.window_complete {
                    let new_window_start = info.window_start;
                    info!(
                        "🔄 Starting new window for {} from {}",
                        self.relay_url,
                        Self::format_timestamp(new_window_start)
                    );
                    (new_window_start, Timestamp::now())
                } else {
                    info!(
                        "▶️  Continuing window for {} with until={}",
                        self.relay_url,
                        Self::format_timestamp(info.last_until_timestamp)
                    );
                    (info.window_start, info.last_until_timestamp)
                }
            } else {
                let now = Timestamp::now();
                info!("🏁 Starting first window for {} from now", self.relay_url);
                (now, now)
            }
        };

        let mut page_number = 0u64;
        let mut consecutive_empty_pages = 0u32;

        loop {
            page_number += 1;
            if self.cancellation_token.is_cancelled() {
                break;
            }

            // Create filters for backward pagination
            let mut page_filters = self.config.filters.clone();
            for filter in &mut page_filters {
                filter.since = None;
                filter.until = Some(last_until);
                filter.limit = Some(self.config.page_size);
            }

            info!(
                "📄 Fetching page #{} from {} with until={}",
                page_number,
                self.relay_url,
                Self::format_timestamp(last_until)
            );

            // Fetch events
            let events = match self.fetch_events_page(&client, page_filters).await {
                Ok(events) => events,
                Err(e) => {
                    error!("Failed to fetch page: {}, retrying...", e);
                    sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };

            let event_count = events.len();

            if event_count > 0 {
                consecutive_empty_pages = 0;

                // Process events in parallel using the event processor pool
                let mut oldest_timestamp = Timestamp::now();

                // Find oldest timestamp
                for event in &events {
                    if event.created_at < oldest_timestamp {
                        oldest_timestamp = event.created_at;
                    }
                }

                // Submit all events to the pool at once
                if let Err(e) = self
                    .validation_pool
                    .submit_profiles(events, nostr_lmdb::Scope::Default)
                    .await
                {
                    error!("Failed to submit profiles to validation pool: {}", e);
                    continue;
                }

                // Update state
                let total_events = {
                    let mut state_guard = self.state.write().await;
                    let info = state_guard
                        .relay_states
                        .entry(self.relay_url.clone())
                        .or_insert_with(|| RelayPaginationInfo {
                            window_start,
                            last_until_timestamp: last_until,
                            total_events_fetched: 0,
                            window_complete: false,
                        });
                    info.total_events_fetched += event_count as u64;
                    info.total_events_fetched
                };

                info!(
                    "📦 Submitted {} events from page #{} to processing pool (total: {}) from {}",
                    event_count, page_number, total_events, self.relay_url
                );

                // Update pagination timestamp
                // Protection against pages that don't change their until value
                if oldest_timestamp == last_until {
                    // Subtract one second to avoid getting stuck
                    last_until = Timestamp::from(oldest_timestamp.as_u64().saturating_sub(1));
                    warn!(
                        "⚠️  Page returned same timestamp as last_until ({}), subtracting 1 second to avoid infinite loop",
                        oldest_timestamp
                    );
                } else {
                    last_until = oldest_timestamp;
                }

                let mut state_guard = self.state.write().await;
                if let Some(info) = state_guard.relay_states.get_mut(&self.relay_url) {
                    info.last_until_timestamp = last_until;
                }
            } else {
                consecutive_empty_pages += 1;
                if consecutive_empty_pages >= 3 {
                    info!("✅ Window complete for {}", self.relay_url);

                    let mut state_guard = self.state.write().await;
                    if let Some(info) = state_guard.relay_states.get_mut(&self.relay_url) {
                        info.window_complete = true;
                    }

                    // Start new window
                    last_until = Timestamp::now();
                    page_number = 0;
                    consecutive_empty_pages = 0;
                }
            }
        }

        Ok(())
    }

    async fn fetch_events_page(
        &self,
        client: &Client,
        filters: Vec<Filter>,
    ) -> Result<Vec<Event>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all_events = Vec::new();
        let timeout = Duration::from_secs(30);

        for filter in filters {
            match tokio::time::timeout(
                timeout + Duration::from_secs(10),
                client.stream_events(filter, timeout),
            )
            .await
            {
                Ok(Ok(mut stream)) => {
                    while let Some(event) = stream.next().await {
                        all_events.push(event);
                    }
                }
                Ok(Err(e)) => {
                    warn!("Failed to create stream: {}", e);
                }
                Err(_) => {
                    warn!("Stream timeout");
                }
            }
        }

        Ok(all_events)
    }

    fn format_timestamp(ts: Timestamp) -> String {
        DateTime::<Utc>::from_timestamp(ts.as_u64() as i64, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "Invalid timestamp".to_string())
    }
}
