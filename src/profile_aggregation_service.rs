//! Profile Aggregation Service - Aggregates user profiles from external relays
//!
//! This service fetches user profiles from external relays, validates them,
//! and saves accepted profiles to the database for broadcasting.

use crate::profile_quality_filter::ProfileQualityFilter;
use crate::profile_validator::ProfileValidator;
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
use tracing::{debug, error, info, warn};

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

#[derive(Debug, Clone, Copy)]
enum EventSource {
    Historical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayPaginationInfo {
    /// Start of current window
    window_start: Timestamp,
    /// Last until timestamp used (for backward pagination)
    last_until_timestamp: Timestamp,
    /// Total events fetched from this relay
    total_events_fetched: u64,
    /// Events from historical pagination
    historical_events: u64,
    /// Events from real-time subscription
    realtime_events: u64,
    /// Last activity timestamp
    last_activity: Timestamp,
    /// Whether we've completed the current window
    window_complete: bool,
}

/// Service that aggregates high-quality profiles from external relays
pub struct ProfileAggregationService {
    config: ProfileAggregationConfig,
    validator: Arc<ProfileValidator>,
    state: Arc<RwLock<AggregationState>>,
    cancellation_token: CancellationToken,
}

impl ProfileAggregationService {
    /// Create a new profile aggregation service with a quality filter
    pub async fn new(
        config: ProfileAggregationConfig,
        filter: Arc<ProfileQualityFilter>,
        database: Arc<RelayDatabase>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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

        // Add multiple discovery relays for better coverage
        let discovery_relays = vec![
            "wss://relay.nos.social",
            "wss://relay.damus.io",
            "wss://nos.lol",
            "wss://relay.primal.net",
        ];

        for relay in discovery_relays {
            if let Err(e) = gossip_client.add_discovery_relay(relay).await {
                warn!("Failed to add discovery relay {}: {}", relay, e);
            }
        }

        gossip_client.connect().await;

        let gossip_client = Arc::new(gossip_client);

        // Create profile validator
        let validator = Arc::new(ProfileValidator::new(
            filter.clone(),
            database.clone(),
            gossip_client.clone(),
        ));

        Ok(Self {
            config,
            validator,
            state,
            cancellation_token,
        })
    }

    /// Run the aggregation service
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!(
            "Starting profile aggregation service: {} relays",
            self.config.relay_urls.len(),
        );

        // Create tasks for each relay
        let mut relay_handles = Vec::new();

        for relay_url in self.config.relay_urls.clone() {
            let config = self.config.clone();
            let validator = self.validator.clone();
            let state = self.state.clone();
            let cancellation_token = self.cancellation_token.clone();

            let handle = tokio::spawn(async move {
                let harvester = ProfileHarvester {
                    relay_url: relay_url.clone(),
                    config,
                    validator,
                    state,
                    cancellation_token,
                };

                if let Err(e) = harvester.run().await {
                    error!("Profile harvester for {} failed: {}", relay_url, e);
                }
            });

            relay_handles.push(handle);
        }

        // Log metrics periodically (every 20 seconds)
        let mut metrics_interval = interval(Duration::from_secs(20));
        let validator = self.validator.clone();

        // Retry processing interval (every 5 seconds)
        let mut retry_interval = interval(Duration::from_secs(5));
        let retry_validator = self.validator.clone();

        // Cleanup interval (every 5 minutes)
        let mut cleanup_interval = interval(Duration::from_secs(300));
        let cleanup_validator = self.validator.clone();

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
                    let metrics = validator.metrics().await;
                    info!(
                        "Validation metrics - Current: processing={}, delayed_retry_queue={} | Total: processed={}, accepted={}, rejected={}, failed={}, rate_limited={}",
                        metrics.current_processing,
                        metrics.current_delayed_retry_queue,
                        metrics.total_processed,
                        metrics.total_accepted,
                        metrics.total_rejected,
                        metrics.total_failed,
                        metrics.total_rate_limited
                    );

                    if !metrics.top_rate_limited_domains.is_empty() {
                        info!(
                            "Recently rate-limited domains (last 5 min): {}",
                            metrics.top_rate_limited_domains
                                .iter()
                                .map(|(domain, count)| format!("{}: {}", domain, count))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }

                _ = retry_interval.tick() => {
                    let processed = retry_validator.process_ready_retries().await;
                    if processed > 0 {
                        debug!("Processed {} delayed retries", processed);
                    }
                }

                _ = cleanup_interval.tick() => {
                    cleanup_validator.cleanup().await;
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
#[derive(Clone)]
struct ProfileHarvester {
    relay_url: String,
    config: ProfileAggregationConfig,
    validator: Arc<ProfileValidator>,
    state: Arc<RwLock<AggregationState>>,
    cancellation_token: CancellationToken,
}

impl ProfileHarvester {
    async fn run(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    /// Record events processed from either source
    async fn record_events_processed(&self, source: EventSource, count: usize) {
        let mut state_guard = self.state.write().await;
        let info = state_guard
            .relay_states
            .entry(self.relay_url.clone())
            .or_insert_with(|| RelayPaginationInfo {
                window_start: Timestamp::now(),
                last_until_timestamp: Timestamp::now(),
                total_events_fetched: 0,
                historical_events: 0,
                realtime_events: 0,
                last_activity: Timestamp::now(),
                window_complete: false,
            });

        match source {
            EventSource::Historical => {
                info.historical_events += count as u64;
            }
        }

        info.total_events_fetched += count as u64;
        info.last_activity = Timestamp::now();
    }

    async fn connect_and_harvest(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

        // Record the timestamp when we start to ensure no gaps
        let realtime_start = Timestamp::now();

        // Start real-time subscription for new events in the background
        {
            let client = client.clone();
            let validator = self.validator.clone();
            let relay_url = self.relay_url.clone();
            let cancellation_token = self.cancellation_token.clone();
            let filters = self.config.filters.clone();
            let state = self.state.clone();

            tokio::spawn(async move {
                if let Err(e) = Self::run_realtime_subscription(
                    client,
                    validator,
                    relay_url,
                    realtime_start,
                    filters,
                    cancellation_token,
                    state,
                )
                .await
                {
                    error!("Real-time subscription error: {}", e);
                }
            });
        }

        // Run backward pagination (the main task)
        self.run_backward_pagination(client).await
    }

    async fn run_backward_pagination(
        &self,
        client: Client,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            // Get or initialize pagination state
            let (_window_start, start_time) = {
                let state_guard = self.state.read().await;
                if let Some(info) = state_guard.relay_states.get(&self.relay_url) {
                    if info.window_complete {
                        let new_window_start = info.window_start;
                        info!(
                            "Starting new window for {} from {}",
                            self.relay_url,
                            Self::format_timestamp(new_window_start)
                        );
                        (new_window_start, Timestamp::now())
                    } else {
                        info!(
                            "Continuing window for {} until {}",
                            self.relay_url,
                            Self::format_timestamp(info.last_until_timestamp)
                        );
                        (info.window_start, info.last_until_timestamp)
                    }
                } else {
                    let now = Timestamp::now();
                    info!("Starting first window for {}", self.relay_url);
                    (now, now)
                }
            };

            // Run pagination directly
            let (last_timestamp, total_events) = self
                .paginate_time_range(
                    client.clone(),
                    start_time,
                    Timestamp::from(0), // Go all the way back to timestamp 0
                )
                .await?;

            // Update final state with last timestamp
            {
                let mut state_guard = self.state.write().await;
                if let Some(info) = state_guard.relay_states.get_mut(&self.relay_url) {
                    info.last_until_timestamp = last_timestamp;
                    // Mark window complete if we hit empty pages (handled by paginate_time_range)
                    if last_timestamp > Timestamp::from(0) {
                        info.window_complete = true;
                        info!(
                            "Window complete for {} - processed {} events, stopped at {}",
                            self.relay_url,
                            total_events,
                            Self::format_timestamp(last_timestamp)
                        );
                    }
                }
            }

            // Check if we should continue with a new window
            if self.cancellation_token.is_cancelled() {
                break;
            }
        }

        Ok(())
    }

    /// Paginate through a specific time range, processing events directly
    ///
    /// # Arguments
    /// * `client` - The Nostr client
    /// * `start_time` - The most recent timestamp to start from (moving backward)
    /// * `end_time` - The oldest timestamp to stop at
    ///
    /// # Returns
    /// * `Ok((last_timestamp, total_events))` - The last timestamp reached and total events processed
    /// * `Err` if pagination was interrupted or failed
    async fn paginate_time_range(
        &self,
        client: Client,
        start_time: Timestamp,
        end_time: Timestamp,
    ) -> Result<(Timestamp, u64), Box<dyn std::error::Error + Send + Sync>> {
        let mut last_until = start_time;
        let mut total_events = 0u64;
        let mut page_number = 0u64;
        let mut consecutive_empty_pages = 0u32;
        let mut consecutive_errors = 0u32;

        info!(
            "Starting pagination for {} from {} to {}",
            self.relay_url,
            Self::format_timestamp(start_time),
            Self::format_timestamp(end_time)
        );

        loop {
            page_number += 1;

            // Check for cancellation
            if self.cancellation_token.is_cancelled() {
                info!("Pagination cancelled for {}", self.relay_url);
                break;
            }

            // Check if we've reached the end time
            if last_until <= end_time {
                info!(
                    "Reached end time {} for {}",
                    Self::format_timestamp(end_time),
                    self.relay_url
                );
                break;
            }

            // Create filters for this page
            let mut page_filters = self.config.filters.clone();
            for filter in &mut page_filters {
                filter.since = Some(end_time); // Don't go older than end_time
                filter.until = Some(last_until);
                filter.limit = Some(self.config.page_size);
            }

            debug!(
                "Fetching page {} from {} (until: {})",
                page_number,
                self.relay_url,
                Self::format_timestamp(last_until)
            );

            // Fetch events with retry logic
            let events = match self.fetch_events_page(&client, page_filters).await {
                Ok(events) => {
                    consecutive_errors = 0;
                    events
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!(
                        "Failed to fetch page from {}: {} (attempt {})",
                        self.relay_url, e, consecutive_errors
                    );

                    if consecutive_errors >= 3 {
                        return Err(Box::new(std::io::Error::other(format!(
                            "Failed after {} consecutive errors",
                            consecutive_errors
                        ))));
                    }

                    let backoff = Duration::from_secs(30 * consecutive_errors as u64);
                    sleep(backoff).await;
                    continue;
                }
            };

            let event_count = events.len();

            // Handle empty pages
            if event_count == 0 {
                consecutive_empty_pages += 1;
                debug!(
                    "Empty page {} from {} (consecutive: {})",
                    page_number, self.relay_url, consecutive_empty_pages
                );

                if consecutive_empty_pages >= 3 {
                    info!(
                        "No more events found for {} after {} empty pages",
                        self.relay_url, consecutive_empty_pages
                    );
                    break;
                }
            } else {
                consecutive_empty_pages = 0;
                total_events += event_count as u64;

                // Process events directly through validator
                if let Err(e) = self
                    .validator
                    .process_profiles(events.clone(), nostr_lmdb::Scope::Default)
                    .await
                {
                    error!("Failed to process profiles: {}", e);
                    // Continue with next batch instead of failing completely
                    continue;
                }

                // Track events processed
                self.record_events_processed(EventSource::Historical, event_count)
                    .await;

                // Update last_until to the oldest timestamp from this batch
                let oldest_timestamp = events
                    .iter()
                    .map(|e| e.created_at)
                    .min()
                    .unwrap_or(last_until);

                // Protection against pagination getting stuck
                if oldest_timestamp == last_until && event_count > 0 {
                    warn!(
                        "Pagination stuck at {} for {}, adjusting timestamp",
                        Self::format_timestamp(last_until),
                        self.relay_url
                    );
                    last_until = Timestamp::from(last_until.as_u64().saturating_sub(1));
                } else {
                    last_until = oldest_timestamp;
                }

                // Log pagination progress less frequently
                if page_number % 100 == 0 {
                    debug!(
                        "Pagination for {}: page {}, at timestamp {} ({}), {} events in this batch",
                        self.relay_url,
                        page_number,
                        last_until.as_u64(),
                        Self::format_timestamp(last_until),
                        event_count
                    );
                }
            }

            // Small delay between pages to avoid overwhelming the relay
            sleep(Duration::from_millis(100)).await;
        }

        info!(
            "Pagination complete for {}: {} total events, stopped at {}",
            self.relay_url,
            total_events,
            Self::format_timestamp(last_until)
        );

        Ok((last_until, total_events))
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

    /// Run a real-time subscription for new events
    async fn run_realtime_subscription(
        client: Client,
        validator: Arc<ProfileValidator>,
        relay_url: String,
        since: Timestamp,
        filters: Vec<Filter>,
        cancellation_token: CancellationToken,
        state: Arc<RwLock<AggregationState>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "Starting real-time subscription for {} since {}",
            relay_url,
            Self::format_timestamp(since)
        );

        // Create filters for real-time events
        let mut realtime_filters = filters;
        for filter in &mut realtime_filters {
            filter.since = Some(since);
        }

        // Subscribe to real-time events
        let sub_id = client
            .subscribe(realtime_filters[0].clone(), None)
            .await?
            .val;

        // Add any additional filters to the same subscription
        for filter in realtime_filters.into_iter().skip(1) {
            client
                .subscribe_with_id(sub_id.clone(), filter, None)
                .await?;
        }

        info!("Real-time subscription {} active for {}", sub_id, relay_url);

        // Handle incoming events
        let event_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let event_count_clone = event_count.clone();
        let relay_url_for_tracking = relay_url.clone();

        client
            .handle_notifications(|notification| {
                let cancellation_token = cancellation_token.clone();
                let sub_id = sub_id.clone();
                let validator = validator.clone();
                let relay_url = relay_url.clone();
                let event_count_clone = event_count_clone.clone();
                let state = state.clone();
                let relay_url_for_tracking = relay_url_for_tracking.clone();

                async move {
                    if cancellation_token.is_cancelled() {
                        return Ok(true); // Exit the loop
                    }

                    match notification {
                        RelayPoolNotification::Event {
                            subscription_id,
                            event,
                            ..
                        } => {
                            if subscription_id == sub_id {
                                let count = event_count_clone
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    + 1;

                                debug!(
                                    "Real-time event #{} received: kind={}, author={}, created_at={}",
                                    count, event.kind, event.pubkey, event.created_at
                                );

                                // Process event through validator
                                if let Err(e) = validator
                                    .process_profiles(vec![*event], nostr_lmdb::Scope::Default)
                                    .await
                                {
                                    error!("Failed to process real-time event: {}", e);
                                } else {
                                    // Track real-time event
                                    let mut state_guard = state.write().await;
                                    if let Some(info) = state_guard.relay_states.get_mut(&relay_url_for_tracking) {
                                        info.realtime_events += 1;
                                        info.total_events_fetched += 1;
                                        info.last_activity = Timestamp::now();

                                        if info.realtime_events % 100 == 0 {
                                            debug!(
                                                "Real-time progress for {}: {} events",
                                                relay_url_for_tracking, info.realtime_events
                                            );
                                        }
                                    }
                                }

                                if count % 100 == 0 {
                                    info!(
                                        "Real-time subscription {}: processed {} events",
                                        relay_url, count
                                    );
                                }
                            } else {
                                debug!("Received event for different subscription: {} != {}", subscription_id, sub_id);
                            }
                        }
                        RelayPoolNotification::Message { message, relay_url: msg_relay } => {
                            debug!("Real-time subscription received message from {}: {:?}", msg_relay, message);
                        }
                        _ => {
                            // Other notification types
                        }
                    }

                    Ok(false) // Continue processing
                }
            })
            .await?;

        let final_count = event_count.load(std::sync::atomic::Ordering::Relaxed);

        info!(
            "Real-time subscription ended for {} (processed {} events)",
            relay_url, final_count
        );

        Ok(())
    }
}
