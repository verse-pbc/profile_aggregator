//! Direct profile validation without worker pool

use crate::profile_quality_filter::ProfileQualityFilter;
use nostr_relay_builder::DatabaseSender;
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Metrics for profile validation
#[derive(Debug)]
pub struct ProfileValidatorMetrics {
    // Current state (snapshots)
    pub current_processing: AtomicUsize,
    pub current_delayed_retry_queue: AtomicUsize,

    // Cumulative counters
    pub total_processed: AtomicU64,
    pub total_accepted: AtomicU64,
    pub total_rejected: AtomicU64,
    pub total_failed: AtomicU64,
    pub total_rate_limited: AtomicU64,
    pub total_max_retries_exceeded: AtomicU64,

    // Domain-specific rate limit tracking (domain -> (count, last_hit_time))
    pub rate_limited_domains: Arc<RwLock<HashMap<String, (u64, Instant)>>>,
}

impl Default for ProfileValidatorMetrics {
    fn default() -> Self {
        Self {
            current_processing: AtomicUsize::new(0),
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
    pub current_processing: usize,
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

/// Delayed retry entry
#[derive(Debug, Clone)]
pub struct DelayedRetry {
    pub event: Event,
    pub scope: nostr_lmdb::Scope,
    pub retry_count: u8,
    pub retry_time: Instant,
}

/// Direct profile validator without worker pool
pub struct ProfileValidator {
    filter: Arc<ProfileQualityFilter>,
    db_sender: DatabaseSender,
    gossip_client: Arc<Client>,
    metrics: Arc<ProfileValidatorMetrics>,
    delayed_retries: Arc<RwLock<Vec<DelayedRetry>>>,
}

impl ProfileValidator {
    /// Create a new profile validator
    pub fn new(
        filter: Arc<ProfileQualityFilter>,
        db_sender: DatabaseSender,
        gossip_client: Arc<Client>,
    ) -> Self {
        Self {
            filter,
            db_sender,
            gossip_client,
            metrics: Arc::new(ProfileValidatorMetrics::default()),
            delayed_retries: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Shutdown the validator and its resources
    pub async fn shutdown(&self) {
        debug!("Shutting down profile validator");
        self.gossip_client.shutdown().await;
    }

    /// Process a batch of profile events
    pub async fn process_profiles(
        &self,
        events: Vec<Event>,
        scope: nostr_lmdb::Scope,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for event in events {
            self.process_single_profile(event, scope.clone(), 0).await;
        }
        Ok(())
    }

    /// Process a single profile
    async fn process_single_profile(
        &self,
        event: Event,
        scope: nostr_lmdb::Scope,
        retry_count: u8,
    ) {
        self.metrics
            .current_processing
            .fetch_add(1, AtomicOrdering::Relaxed);

        let result = self
            .filter
            .apply_filter_with_gossip(event.clone(), scope.clone(), &self.gossip_client)
            .await;

        self.metrics
            .current_processing
            .fetch_sub(1, AtomicOrdering::Relaxed);
        self.metrics
            .total_processed
            .fetch_add(1, AtomicOrdering::Relaxed);

        match result {
            Ok(commands) => {
                if commands.is_empty() {
                    self.metrics
                        .total_rejected
                        .fetch_add(1, AtomicOrdering::Relaxed);
                } else {
                    self.metrics
                        .total_accepted
                        .fetch_add(1, AtomicOrdering::Relaxed);
                }

                // Save to database
                for cmd in commands {
                    if let Err(e) = self.db_sender.send(cmd).await {
                        warn!("Failed to save event: {}", e);
                    }
                }
            }
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains("Rate limited") {
                    self.handle_rate_limit(event, scope, retry_count, &error_msg)
                        .await;
                } else {
                    self.metrics
                        .total_failed
                        .fetch_add(1, AtomicOrdering::Relaxed);
                    warn!("Failed to process event: {}", e);
                }
            }
        }
    }

    /// Handle rate-limited event
    async fn handle_rate_limit(
        &self,
        event: Event,
        scope: nostr_lmdb::Scope,
        retry_count: u8,
        error_msg: &str,
    ) {
        self.metrics
            .total_rate_limited
            .fetch_add(1, AtomicOrdering::Relaxed);

        // Extract domain from error message
        if let Some(start) = error_msg.find("Rate limited by ") {
            let domain_part = &error_msg[start + 16..];
            if let Some(end) = domain_part.find(':').or(Some(domain_part.len())) {
                let domain = domain_part[..end].to_string();
                debug!("Rate limit hit for domain: {}", domain);

                // Track domain-specific rate limits
                let mut domains_guard = self.metrics.rate_limited_domains.write().await;
                let entry = domains_guard.entry(domain).or_insert((0, Instant::now()));
                entry.0 += 1;
                entry.1 = Instant::now();
            }
        }

        // Add to delayed retry if under retry limit
        if retry_count < 3 {
            let base_delay = Duration::from_secs(30);
            let retry_delay = base_delay * (1 << retry_count); // Exponential backoff
            let retry_time = Instant::now() + retry_delay;

            let mut retries = self.delayed_retries.write().await;
            retries.push(DelayedRetry {
                event,
                scope,
                retry_count: retry_count + 1,
                retry_time,
            });

            self.metrics
                .current_delayed_retry_queue
                .store(retries.len(), AtomicOrdering::Relaxed);

            debug!(
                "Queued rate-limited event for retry #{} at {:?}",
                retry_count + 1,
                retry_time
            );
        } else {
            self.metrics
                .total_max_retries_exceeded
                .fetch_add(1, AtomicOrdering::Relaxed);
            self.metrics
                .total_failed
                .fetch_add(1, AtomicOrdering::Relaxed);
            debug!("Giving up on event after {} retries", retry_count);
        }
    }

    /// Process any delayed retries that are ready
    pub async fn process_ready_retries(&self) -> usize {
        let now = Instant::now();
        let mut retries = self.delayed_retries.write().await;

        // Find retries that are ready
        let mut ready_retries = Vec::new();
        retries.retain(|retry| {
            if retry.retry_time <= now {
                ready_retries.push(retry.clone());
                false
            } else {
                true
            }
        });

        self.metrics
            .current_delayed_retry_queue
            .store(retries.len(), AtomicOrdering::Relaxed);
        drop(retries);

        let count = ready_retries.len();

        // Process ready retries
        for retry in ready_retries {
            self.process_single_profile(retry.event, retry.scope, retry.retry_count)
                .await;
        }

        count
    }

    /// Clean up old delayed retries and rate limit tracking
    pub async fn cleanup(&self) {
        let mut retries = self.delayed_retries.write().await;
        let before_size = retries.len();

        // Remove events older than 1 hour
        const MAX_EVENT_AGE: Duration = Duration::from_secs(3600);
        let now = Instant::now();
        retries.retain(|retry| {
            // Calculate age based on when it was originally queued
            // Calculate age - retries are scheduled in the future, so we need to calculate backwards
            let scheduled_delay = match retry.retry_count {
                1 => Duration::from_secs(30),
                2 => Duration::from_secs(120),
                3 => Duration::from_secs(480),
                _ => Duration::from_secs(30),
            };
            let original_queue_time = retry
                .retry_time
                .checked_sub(scheduled_delay)
                .unwrap_or(retry.retry_time);
            let age = now.duration_since(original_queue_time);
            age < MAX_EVENT_AGE
        });

        let removed = before_size - retries.len();
        if removed > 0 {
            info!("Cleaned up {} expired events from delayed queue", removed);
            self.metrics
                .current_delayed_retry_queue
                .store(retries.len(), AtomicOrdering::Relaxed);
        }

        // Warn if queue is getting large
        const MAX_DELAYED_QUEUE_SIZE: usize = 10000;
        if retries.len() >= MAX_DELAYED_QUEUE_SIZE * 9 / 10 {
            warn!(
                "Delayed retry queue is {}% full ({}/{} events)",
                (retries.len() * 100) / MAX_DELAYED_QUEUE_SIZE,
                retries.len(),
                MAX_DELAYED_QUEUE_SIZE
            );
        }
        drop(retries);

        // Clean up rate-limited domains
        let mut domains_guard = self.metrics.rate_limited_domains.write().await;
        let now = Instant::now();
        // Remove domains that haven't been rate limited in the last hour
        domains_guard.retain(|_, (count, last_hit)| {
            *count >= 10 && now.duration_since(*last_hit) < Duration::from_secs(3600)
        });
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
            current_processing: self
                .metrics
                .current_processing
                .load(AtomicOrdering::Relaxed),
            current_delayed_retry_queue: self
                .metrics
                .current_delayed_retry_queue
                .load(AtomicOrdering::Relaxed),
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
