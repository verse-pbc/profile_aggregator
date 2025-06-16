use crate::profile_quality_filter::ProfileQualityFilter;
use nostr_lmdb::Scope;
use nostr_relay_builder::RelayDatabase;
use nostr_sdk::prelude::*;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
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
#[derive(Debug)]
struct ProfileValidatorMetrics {
    // Current state (snapshots)
    current_queued: AtomicUsize,
    current_delayed_retry_queue: AtomicUsize,

    // Cumulative counters
    total_processed: AtomicU64,
    total_accepted: AtomicU64,
    total_rejected: AtomicU64,
    total_failed: AtomicU64,
    total_rate_limited: AtomicU64,
    total_max_retries_exceeded: AtomicU64,

    // Domain-specific rate limit tracking (domain -> (count, last_hit_time))
    rate_limited_domains: Arc<RwLock<HashMap<String, (u64, Instant)>>>,
}

impl Default for ProfileValidatorMetrics {
    fn default() -> Self {
        Self {
            current_queued: AtomicUsize::new(0),
            current_delayed_retry_queue: AtomicUsize::new(0),
            total_processed: AtomicU64::new(0),
            total_accepted: AtomicU64::new(0),
            total_rejected: AtomicU64::new(0),
            total_failed: AtomicU64::new(0),
            total_rate_limited: AtomicU64::new(0),
            total_max_retries_exceeded: AtomicU64::new(0),
            rate_limited_domains: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

/// Snapshot of metrics for reporting
#[derive(Debug, Clone)]
pub struct ProfileValidatorMetricsSnapshot {
    // Current state (snapshots)
    pub current_queued: usize,
    pub current_delayed_retry_queue: usize,

    // Cumulative counters
    pub total_processed: u64,
    pub total_accepted: u64,
    pub total_rejected: u64,
    pub total_failed: u64,
    pub total_rate_limited: u64,
    pub total_max_retries_exceeded: u64,

    // Top rate-limited domains
    pub top_rate_limited_domains: Vec<(String, u64)>,
}

/// Context for processing profile validation requests
struct ProcessContext<'a> {
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
    // Task tracker for graceful shutdown
    #[allow(dead_code)]
    task_tracker: TaskTracker,
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
        let task_tracker = TaskTracker::new();

        info!(
            "Starting profile validation pool with {} workers, queue size {}",
            worker_count, queue_size
        );

        // Spawn worker tasks
        for worker_id in 0..worker_count {
            let immediate_rx = immediate_rx.clone();
            let delayed_queue = delayed_queue.clone();
            let database = database.clone();
            let filter = filter.clone();
            let metrics = metrics.clone();
            let cancel = cancellation_token.clone();
            let gossip_client = Some(gossip_client.clone());

            task_tracker.spawn(async move {
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
        }

        // Spawn a cleanup task for the delayed queue
        {
            let delayed_queue = delayed_queue.clone();
            let metrics = metrics.clone();
            let cancel = cancellation_token.clone();

            task_tracker.spawn(async move {
                let mut cleanup_interval = tokio::time::interval(Duration::from_secs(300)); // Every 5 minutes
                cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    tokio::select! {
                        _ = cleanup_interval.tick() => {
                            let mut queue = delayed_queue.lock().await;
                            let before_size = queue.len();

                            // Remove events older than 1 hour
                            const MAX_EVENT_AGE: Duration = Duration::from_secs(3600);
                            queue.retain(|delayed| {
                                delayed.request.age() < MAX_EVENT_AGE
                            });

                            let removed = before_size - queue.len();
                            if removed > 0 {
                                info!("Cleaned up {} expired events from delayed queue", removed);
                                metrics.current_delayed_retry_queue.store(queue.len(), AtomicOrdering::Relaxed);
                            }

                            // Check if queue is at or near capacity
                            const MAX_DELAYED_QUEUE_SIZE: usize = 10000;
                            if queue.len() >= MAX_DELAYED_QUEUE_SIZE * 9 / 10 {
                                warn!(
                                    "Delayed retry queue is {}% full ({}/{} events). Consider increasing worker threads or queue size.",
                                    (queue.len() * 100) / MAX_DELAYED_QUEUE_SIZE,
                                    queue.len(),
                                    MAX_DELAYED_QUEUE_SIZE
                                );
                            }

                            // Also clean up rate-limited domains periodically
                            let domains = metrics.rate_limited_domains.clone();
                            tokio::spawn(async move {
                                let mut domains_guard = domains.write().await;
                                let now = Instant::now();
                                // Remove domains that haven't been rate limited in the last hour
                                domains_guard.retain(|_, (count, last_hit)| {
                                    *count >= 10 && now.duration_since(*last_hit) < Duration::from_secs(3600)
                                });
                            });
                        }

                        _ = cancel.cancelled() => {
                            debug!("Delayed queue cleanup task cancelled");
                            break;
                        }
                    }
                }
            });
        }

        Self {
            immediate_tx,
            delayed_queue,
            metrics,
            task_tracker,
        }
    }

    /// Process a single validation request
    async fn process_request(
        req: ProfileValidationRequest,
        ctx: &ProcessContext<'_>,
        worker_id: usize,
    ) {
        // Update metrics - use saturating_sub to prevent underflow
        let _ = ctx.metrics.current_queued.fetch_update(
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
            |val| val.checked_sub(1),
        );
        ctx.metrics
            .total_processed
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
                        .total_rejected
                        .fetch_add(1, AtomicOrdering::Relaxed);
                } else {
                    ctx.metrics
                        .total_accepted
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
                let error_msg = e.to_string();
                if error_msg.contains("Rate limited") {
                    ctx.metrics
                        .total_rate_limited
                        .fetch_add(1, AtomicOrdering::Relaxed);

                    // Extract domain from error message (format: "Rate limited by domain.com")
                    if let Some(start) = error_msg.find("Rate limited by ") {
                        let domain_part = &error_msg[start + 16..];
                        if let Some(end) = domain_part.find(':').or(Some(domain_part.len())) {
                            let domain = domain_part[..end].to_string();
                            debug!("Rate limit hit for domain: {}", domain);

                            // Track domain-specific rate limits
                            let domains = ctx.metrics.rate_limited_domains.clone();
                            tokio::spawn(async move {
                                let mut domains_guard = domains.write().await;
                                let entry =
                                    domains_guard.entry(domain).or_insert((0, Instant::now()));
                                entry.0 += 1;
                                entry.1 = Instant::now();
                            });
                        }
                    }

                    // Check retry count
                    if req.retry_count < 3 {
                        // Use exponential backoff: 30s, 2min, 8min
                        let base_delay = Self::parse_retry_delay(&e.to_string())
                            .unwrap_or(Duration::from_secs(30));
                        let retry_delay = base_delay * (1 << req.retry_count); // Exponential backoff

                        let retry_time = Instant::now() + retry_delay;
                        let retry_count = req.retry_count;

                        // Add to delayed queue with size and age limits
                        let mut queue = ctx.delayed_queue.lock().await;

                        // Limit queue size to prevent CPU thrashing
                        const MAX_DELAYED_QUEUE_SIZE: usize = 10000;

                        // If at capacity, drop oldest events (1/3 of queue) to make room
                        if queue.len() >= MAX_DELAYED_QUEUE_SIZE {
                            // Convert to Vec for efficient sorting and truncation
                            let mut items: Vec<_> = queue.drain().collect();

                            // Sort by retry time (oldest first)
                            items.sort_by_key(|d| d.earliest_retry_time);

                            // Drop the oldest 1/3
                            let to_keep = (items.len() * 2) / 3;
                            let dropped = items.len() - to_keep;
                            items.truncate(to_keep);

                            // Rebuild the heap
                            *queue = items.into_iter().collect();

                            ctx.metrics
                                .total_max_retries_exceeded
                                .fetch_add(dropped as u64, AtomicOrdering::Relaxed);
                        }

                        // Add the new event
                        let delayed_req = DelayedRequest {
                            earliest_retry_time: retry_time,
                            request: req.with_retry(retry_time),
                        };
                        queue.push(delayed_req);
                        ctx.metrics
                            .current_delayed_retry_queue
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
                            .total_max_retries_exceeded
                            .fetch_add(1, AtomicOrdering::Relaxed);
                        ctx.metrics
                            .total_failed
                            .fetch_add(1, AtomicOrdering::Relaxed);
                        debug!(
                            "Worker {} giving up on event after {} retries",
                            worker_id, req.retry_count
                        );
                    }
                } else {
                    // Other error
                    ctx.metrics
                        .total_failed
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
                    .current_delayed_retry_queue
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
                .current_queued
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
    pub async fn metrics(&self) -> ProfileValidatorMetricsSnapshot {
        // Get top rate-limited domains (only those hit in the last 5 minutes)
        let domains_guard = self.metrics.rate_limited_domains.read().await;
        let now = Instant::now();
        let recent_window = Duration::from_secs(300); // 5 minutes

        let mut domain_counts: Vec<(String, u64)> = domains_guard
            .iter()
            .filter(|(_, (_, last_hit))| now.duration_since(*last_hit) < recent_window)
            .map(|(k, (count, _))| (k.clone(), *count))
            .collect();
        domain_counts.sort_by(|a, b| b.1.cmp(&a.1));
        let top_domains = domain_counts.into_iter().take(5).collect();
        drop(domains_guard);

        ProfileValidatorMetricsSnapshot {
            // Current state (snapshots)
            current_queued: self.metrics.current_queued.load(AtomicOrdering::Relaxed),
            current_delayed_retry_queue: self
                .metrics
                .current_delayed_retry_queue
                .load(AtomicOrdering::Relaxed),

            // Cumulative counters
            total_processed: self.metrics.total_processed.load(AtomicOrdering::Relaxed),
            total_accepted: self.metrics.total_accepted.load(AtomicOrdering::Relaxed),
            total_rejected: self.metrics.total_rejected.load(AtomicOrdering::Relaxed),
            total_failed: self.metrics.total_failed.load(AtomicOrdering::Relaxed),
            total_rate_limited: self
                .metrics
                .total_rate_limited
                .load(AtomicOrdering::Relaxed),
            total_max_retries_exceeded: self
                .metrics
                .total_max_retries_exceeded
                .load(AtomicOrdering::Relaxed),
            top_rate_limited_domains: top_domains,
        }
    }
}
