use crate::profile_quality_filter::ProfileQualityFilter;
use nostr_lmdb::Scope;
use nostr_relay_builder::RelayDatabase;
use nostr_sdk::prelude::*;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Request to validate a profile with retry metadata
#[derive(Debug, Clone)]
struct ProfileValidationRequest {
    event: Event,
    scope: Scope,
    retry_count: u8,
    earliest_retry_time: Option<Instant>,
    created_at: Instant,
}

impl ProfileValidationRequest {
    fn new(event: Event, scope: Scope) -> Self {
        Self {
            event,
            scope,
            retry_count: 0,
            earliest_retry_time: None,
            created_at: Instant::now(),
        }
    }

    fn with_retry(mut self, retry_time: Instant) -> Self {
        self.retry_count += 1;
        self.earliest_retry_time = Some(retry_time);
        self
    }

    fn age(&self) -> Duration {
        Instant::now().duration_since(self.created_at)
    }
}

/// Wrapper for delayed requests in the priority queue
#[derive(Debug, Clone)]
struct DelayedRequest {
    earliest_retry_time: Instant,
    request: ProfileValidationRequest,
}

impl Eq for DelayedRequest {}

impl PartialEq for DelayedRequest {
    fn eq(&self, other: &Self) -> bool {
        self.earliest_retry_time == other.earliest_retry_time
    }
}

impl Ord for DelayedRequest {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap (earliest time first)
        other.earliest_retry_time.cmp(&self.earliest_retry_time)
    }
}

impl PartialOrd for DelayedRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Metrics for monitoring the profile validation pool
#[derive(Default, Debug)]
struct ProfileValidatorMetrics {
    queued_operations: AtomicUsize,
    delayed_operations: AtomicUsize,
    processed_profiles: AtomicU64,
    accepted_profiles: AtomicU64,
    rejected_profiles: AtomicU64,
    failed_operations: AtomicU64,
    rate_limited_retries: AtomicU64,
    max_retries_exceeded: AtomicU64,
}

/// Snapshot of metrics for reporting
#[derive(Debug, Clone)]
pub struct ProfileValidatorMetricsSnapshot {
    pub queued_operations: usize,
    pub delayed_operations: usize,
    pub processed_profiles: u64,
    pub accepted_profiles: u64,
    pub rejected_profiles: u64,
    pub failed_operations: u64,
    pub rate_limited_retries: u64,
    pub max_retries_exceeded: u64,
}

/// Context for processing profile validation requests
struct ProcessContext<'a> {
    _immediate_tx: &'a flume::Sender<ProfileValidationRequest>,
    delayed_queue: &'a Arc<Mutex<BinaryHeap<DelayedRequest>>>,
    database: &'a Arc<RelayDatabase>,
    filter: &'a Arc<ProfileQualityFilter>,
    metrics: &'a Arc<ProfileValidatorMetrics>,
    gossip_client: Option<&'a Client>,
}

/// Enhanced pool of profile validation workers with retry support
#[derive(Clone)]
pub struct ProfileValidationPool {
    // Channel for immediate processing (flume supports multi-consumer)
    immediate_tx: flume::Sender<ProfileValidationRequest>,
    // Shared priority queue for delayed requests
    #[allow(dead_code)]
    delayed_queue: Arc<Mutex<BinaryHeap<DelayedRequest>>>,
    // Metrics for monitoring
    metrics: Arc<ProfileValidatorMetrics>,
    // Handles to worker tasks
    _handles: Arc<Vec<JoinHandle<()>>>,
}

impl ProfileValidationPool {
    /// Create a new enhanced profile validation pool
    pub fn new(
        worker_count: usize,
        filter: Arc<ProfileQualityFilter>,
        database: Arc<RelayDatabase>,
        cancellation_token: CancellationToken,
        gossip_client: Arc<Client>,
    ) -> Self {
        let queue_size = std::env::var("PROFILE_VALIDATOR_QUEUE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(worker_count * 20);
        let (immediate_tx, immediate_rx) = flume::bounded(queue_size);
        let delayed_queue = Arc::new(Mutex::new(BinaryHeap::new()));
        let metrics = Arc::new(ProfileValidatorMetrics::default());
        let mut handles = Vec::new();

        info!(
            "Starting profile validation pool with {} workers, queue size {}",
            worker_count, queue_size
        );

        // Spawn worker tasks
        for worker_id in 0..worker_count {
            let immediate_rx = immediate_rx.clone();
            let immediate_tx = immediate_tx.clone();
            let delayed_queue = delayed_queue.clone();
            let database = database.clone();
            let filter = filter.clone();
            let metrics = metrics.clone();
            let cancel = cancellation_token.clone();
            let gossip_client = Some(gossip_client.clone());

            let handle = tokio::spawn(async move {
                debug!("Profile validator worker {} started", worker_id);

                loop {
                    if cancel.is_cancelled() {
                        break;
                    }

                    tokio::select! {
                        // Check for immediate work
                        result = immediate_rx.recv_async() => {
                            match result {
                                Ok(req) => {
                                    let ctx = ProcessContext {
                                        _immediate_tx: &immediate_tx,
                                        delayed_queue: &delayed_queue,
                                        database: &database,
                                        filter: &filter,
                                        metrics: &metrics,
                                        gossip_client: gossip_client.as_deref(),
                                    };
                                    Self::process_request(req, &ctx, worker_id).await;
                                }
                                Err(_) => {
                                    debug!("Worker {} shutting down - channel closed", worker_id);
                                    break;
                                }
                            }
                        }

                        // Check for delayed work that's ready
                        _ = Self::wait_for_delayed_ready(&delayed_queue) => {
                            if let Some(req) = Self::pop_ready_delayed(&delayed_queue, &metrics).await {
                                let ctx = ProcessContext {
                                    _immediate_tx: &immediate_tx,
                                    delayed_queue: &delayed_queue,
                                    database: &database,
                                    filter: &filter,
                                    metrics: &metrics,
                                    gossip_client: gossip_client.as_deref(),
                                };
                                Self::process_request(req, &ctx, worker_id).await;
                            }
                        }

                        // Cancellation check
                        _ = cancel.cancelled() => {
                            debug!("Worker {} received cancellation signal", worker_id);
                            break;
                        }
                    }
                }

                debug!("Profile validator worker {} stopped", worker_id);
            });

            handles.push(handle);
        }

        // Spawn a cleanup task for the delayed queue
        {
            let delayed_queue = delayed_queue.clone();
            let metrics = metrics.clone();
            let cancel = cancellation_token.clone();

            let cleanup_handle = tokio::spawn(async move {
                let mut cleanup_interval = tokio::time::interval(Duration::from_secs(3600)); // Every hour
                cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    tokio::select! {
                        _ = cleanup_interval.tick() => {
                            let mut queue = delayed_queue.lock().await;
                            let before_size = queue.len();

                            // Remove events older than 24 hours
                            const MAX_EVENT_AGE: Duration = Duration::from_secs(86400);
                            queue.retain(|delayed| {
                                delayed.request.age() < MAX_EVENT_AGE
                            });

                            let removed = before_size - queue.len();
                            if removed > 0 {
                                info!("Cleaned up {} expired events from delayed queue", removed);
                                metrics.delayed_operations.store(queue.len(), AtomicOrdering::Relaxed);
                            }
                        }

                        _ = cancel.cancelled() => {
                            debug!("Delayed queue cleanup task cancelled");
                            break;
                        }
                    }
                }
            });

            handles.push(cleanup_handle);
        }

        Self {
            immediate_tx,
            delayed_queue,
            metrics,
            _handles: Arc::new(handles),
        }
    }

    /// Process a single validation request
    async fn process_request(
        req: ProfileValidationRequest,
        ctx: &ProcessContext<'_>,
        worker_id: usize,
    ) {
        // Update metrics - use saturating_sub to prevent underflow
        let _ = ctx.metrics.queued_operations.fetch_update(
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
            |val| val.checked_sub(1),
        );
        ctx.metrics
            .processed_profiles
            .fetch_add(1, AtomicOrdering::Relaxed);

        // Process the event
        let result = if let Some(client) = ctx.gossip_client {
            ctx.filter
                .apply_filter_with_gossip(req.event.clone(), req.scope.clone(), client)
                .await
        } else {
            ctx.filter
                .apply_filter(req.event.clone(), req.scope.clone())
                .await
        };

        match result {
            Ok(commands) => {
                if commands.is_empty() {
                    ctx.metrics
                        .rejected_profiles
                        .fetch_add(1, AtomicOrdering::Relaxed);
                } else {
                    ctx.metrics
                        .accepted_profiles
                        .fetch_add(1, AtomicOrdering::Relaxed);
                }

                // Save to database
                for cmd in commands {
                    if let Err(e) = ctx.database.save_store_command(cmd).await {
                        warn!("Worker {} failed to save event: {}", worker_id, e);
                    }
                }
            }
            Err(e) => {
                // Check if it's a rate limit error
                if e.to_string().contains("Rate limited") {
                    ctx.metrics
                        .rate_limited_retries
                        .fetch_add(1, AtomicOrdering::Relaxed);

                    // Check retry count
                    if req.retry_count < 5 {
                        // Parse retry delay from error (or use default)
                        let retry_delay = Self::parse_retry_delay(&e.to_string())
                            .unwrap_or(Duration::from_secs(60));

                        let retry_time = Instant::now() + retry_delay;
                        let retry_count = req.retry_count;
                        let delayed_req = DelayedRequest {
                            earliest_retry_time: retry_time,
                            request: req.with_retry(retry_time),
                        };

                        // Add to delayed queue with size and age limits
                        let mut queue = ctx.delayed_queue.lock().await;

                        // Clean up old events and maintain size limit
                        const MAX_DELAYED_QUEUE_SIZE: usize = 300000; // ~630MB at 2.1KB per event
                        const MAX_EVENT_AGE: Duration = Duration::from_secs(86400); // 24 hours for daily rate limits

                        // Remove old events
                        queue.retain(|delayed| delayed.request.age() < MAX_EVENT_AGE);

                        // If still over capacity, drop oldest events
                        if queue.len() >= MAX_DELAYED_QUEUE_SIZE {
                            // Convert to vec, sort by age, keep newest
                            let mut items: Vec<_> = queue.drain().collect();
                            items.sort_by_key(|d| d.request.created_at);
                            items.reverse(); // Newest first
                            items.truncate(MAX_DELAYED_QUEUE_SIZE - 1); // Leave room for new one
                            *queue = items.into_iter().collect();

                            warn!(
                                "Delayed queue at capacity ({}), dropped oldest events",
                                MAX_DELAYED_QUEUE_SIZE
                            );
                        }

                        queue.push(delayed_req);
                        ctx.metrics
                            .delayed_operations
                            .store(queue.len(), AtomicOrdering::Relaxed);

                        debug!(
                            "Worker {} queued rate-limited event for retry #{} at {:?}",
                            worker_id,
                            retry_count + 1,
                            retry_time
                        );
                    } else {
                        // Max retries exceeded
                        ctx.metrics
                            .max_retries_exceeded
                            .fetch_add(1, AtomicOrdering::Relaxed);
                        ctx.metrics
                            .failed_operations
                            .fetch_add(1, AtomicOrdering::Relaxed);
                        warn!(
                            "Worker {} giving up on event after {} retries",
                            worker_id, req.retry_count
                        );
                    }
                } else {
                    // Other error
                    ctx.metrics
                        .failed_operations
                        .fetch_add(1, AtomicOrdering::Relaxed);
                    warn!("Worker {} failed to process event: {}", worker_id, e);
                }
            }
        }
    }

    /// Wait until the next delayed item is ready
    async fn wait_for_delayed_ready(delayed_queue: &Arc<Mutex<BinaryHeap<DelayedRequest>>>) {
        let sleep_duration = {
            let queue = delayed_queue.lock().await;
            queue
                .peek()
                .map(|item| {
                    let now = Instant::now();
                    if item.earliest_retry_time > now {
                        item.earliest_retry_time - now
                    } else {
                        Duration::from_millis(0)
                    }
                })
                .unwrap_or(Duration::from_secs(3600)) // Sleep 1 hour if queue is empty
        };

        tokio::time::sleep(sleep_duration).await;
    }

    /// Pop a ready item from the delayed queue
    async fn pop_ready_delayed(
        delayed_queue: &Arc<Mutex<BinaryHeap<DelayedRequest>>>,
        metrics: &Arc<ProfileValidatorMetrics>,
    ) -> Option<ProfileValidationRequest> {
        let mut queue = delayed_queue.lock().await;

        if let Some(item) = queue.peek() {
            if item.earliest_retry_time <= Instant::now() {
                let delayed_req = queue.pop().unwrap();
                metrics
                    .delayed_operations
                    .store(queue.len(), AtomicOrdering::Relaxed);
                return Some(delayed_req.request);
            }
        }

        None
    }

    /// Parse retry delay from error message
    fn parse_retry_delay(error_msg: &str) -> Option<Duration> {
        // Look for patterns like "retry after 60s" or "wait 120 seconds"
        if let Some(pos) = error_msg.find("retry after ") {
            let after = &error_msg[pos + 12..];
            if let Some(end) = after.find('s') {
                if let Ok(seconds) = after[..end].trim().parse::<u64>() {
                    return Some(Duration::from_secs(seconds));
                }
            }
        }
        None
    }

    /// Submit profiles for processing
    pub async fn submit_profiles(
        &self,
        events: Vec<Event>,
        scope: Scope,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for event in events {
            // Update metrics
            self.metrics
                .queued_operations
                .fetch_add(1, AtomicOrdering::Relaxed);

            // Create new request
            let request = ProfileValidationRequest::new(event, scope.clone());

            // Send to immediate queue
            self.immediate_tx
                .send_async(request)
                .await
                .map_err(|_| "Profile validation queue full")?;
        }

        Ok(())
    }

    /// Get current metrics snapshot
    pub fn metrics(&self) -> ProfileValidatorMetricsSnapshot {
        let delayed_count = {
            // This is approximate since we can't lock in a sync context
            // In practice, you might want to track this differently
            0 // Placeholder
        };

        ProfileValidatorMetricsSnapshot {
            queued_operations: self.metrics.queued_operations.load(AtomicOrdering::Relaxed),
            delayed_operations: delayed_count,
            processed_profiles: self
                .metrics
                .processed_profiles
                .load(AtomicOrdering::Relaxed),
            accepted_profiles: self.metrics.accepted_profiles.load(AtomicOrdering::Relaxed),
            rejected_profiles: self.metrics.rejected_profiles.load(AtomicOrdering::Relaxed),
            failed_operations: self.metrics.failed_operations.load(AtomicOrdering::Relaxed),
            rate_limited_retries: self
                .metrics
                .rate_limited_retries
                .load(AtomicOrdering::Relaxed),
            max_retries_exceeded: self
                .metrics
                .max_retries_exceeded
                .load(AtomicOrdering::Relaxed),
        }
    }
}
