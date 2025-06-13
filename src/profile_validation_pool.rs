use crate::profile_quality_filter::ProfileQualityFilter;
use nostr_lmdb::Scope;
use nostr_relay_builder::RelayDatabase;
use nostr_sdk::prelude::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Request to validate a profile
#[derive(Debug)]
struct ProfileValidationRequest {
    event: Event,
    scope: Scope,
}

/// Metrics for monitoring the profile validation pool
#[derive(Default, Debug)]
struct ProfileValidatorMetrics {
    queued_operations: AtomicUsize,
    processed_profiles: AtomicU64,
    accepted_profiles: AtomicU64,
    rejected_profiles: AtomicU64,
    failed_operations: AtomicU64,
}

/// Snapshot of metrics for reporting
#[derive(Debug, Clone)]
pub struct ProfileValidatorMetricsSnapshot {
    pub queued_operations: usize,
    pub processed_profiles: u64,
    pub accepted_profiles: u64,
    pub rejected_profiles: u64,
    pub failed_operations: u64,
}

/// Pool of profile validation workers for parallel processing
#[derive(Clone)]
pub struct ProfileValidationPool {
    // Channel for sending event requests (flume supports multi-consumer)
    tx: flume::Sender<ProfileValidationRequest>,
    // Metrics for monitoring
    metrics: Arc<ProfileValidatorMetrics>,
    // Handles to worker tasks (kept to ensure they're not dropped)
    _handles: Arc<Vec<JoinHandle<()>>>,
}

impl ProfileValidationPool {
    /// Create a new profile validation pool with the specified number of workers
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

        info!(
            "Starting profile validation pool with {} workers, queue size {}",
            worker_count, queue_size
        );

        // Create a bounded channel for backpressure
        let (tx, rx) = flume::bounded::<ProfileValidationRequest>(queue_size);
        let metrics = Arc::new(ProfileValidatorMetrics::default());
        let mut handles = Vec::new();

        // Spawn worker tasks - they all share the same receiver
        for worker_id in 0..worker_count {
            let rx = rx.clone();
            let filter = filter.clone();
            let database = database.clone();
            let cancel = cancellation_token.clone();
            let metrics = Arc::clone(&metrics);
            let gossip_client = gossip_client.clone();

            let handle = tokio::spawn(async move {
                debug!("Profile validator worker {} started", worker_id);

                loop {
                    // Check cancellation
                    if cancel.is_cancelled() {
                        break;
                    }

                    // Try to get work from the shared queue
                    match rx.recv_async().await {
                        Ok(req) => {
                            // Update metrics
                            metrics.queued_operations.fetch_sub(1, Ordering::Relaxed);
                            metrics.processed_profiles.fetch_add(1, Ordering::Relaxed);

                            // Process the event with gossip client
                            match filter.apply_filter_with_gossip(
                                req.event.clone(), 
                                req.scope.clone(), 
                                gossip_client.as_ref()
                            ).await {
                                Ok(commands) => {
                                    // Successfully processed
                                    if commands.is_empty() {
                                        metrics.rejected_profiles.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        metrics.accepted_profiles.fetch_add(1, Ordering::Relaxed);
                                    }

                                    // Save to database
                                    for cmd in commands {
                                        if let Err(e) = database.save_store_command(cmd).await {
                                            warn!(
                                                "Worker {} failed to save event: {}",
                                                worker_id, e
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    // Processing failed
                                    metrics.failed_operations.fetch_add(1, Ordering::Relaxed);
                                    warn!("Worker {} failed to process event: {}", worker_id, e);
                                }
                            }
                        }
                        Err(_) => {
                            // Channel closed, exit
                            debug!("Worker {} shutting down - channel closed", worker_id);
                            break;
                        }
                    }
                }

                debug!("Profile validator worker {} stopped", worker_id);
            });

            handles.push(handle);
        }

        Self {
            tx,
            metrics,
            _handles: Arc::new(handles),
        }
    }

    /// Submit profiles for processing without waiting for responses
    /// This is fire-and-forget - workers will save directly to database
    pub async fn submit_profiles(
        &self,
        events: Vec<Event>,
        scope: Scope,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for event in events {
            // Update metrics
            self.metrics
                .queued_operations
                .fetch_add(1, Ordering::Relaxed);

            // Send the request to the worker pool
            let request = ProfileValidationRequest {
                event,
                scope: scope.clone(),
            };

            self.tx
                .send_async(request)
                .await
                .map_err(|_| "Profile validation queue full")?;
        }

        Ok(())
    }

    /// Get current metrics
    pub fn metrics(&self) -> ProfileValidatorMetricsSnapshot {
        ProfileValidatorMetricsSnapshot {
            queued_operations: self.metrics.queued_operations.load(Ordering::Relaxed),
            processed_profiles: self.metrics.processed_profiles.load(Ordering::Relaxed),
            accepted_profiles: self.metrics.accepted_profiles.load(Ordering::Relaxed),
            rejected_profiles: self.metrics.rejected_profiles.load(Ordering::Relaxed),
            failed_operations: self.metrics.failed_operations.load(Ordering::Relaxed),
        }
    }
}

impl Drop for ProfileValidationPool {
    fn drop(&mut self) {
        // Dropping tx will signal workers to shut down
    }
}
