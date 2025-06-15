use crate::domain::{ProfileClaim, ProcessedEvent};
use crate::media_error::MediaError;
use crate::profile_claim_extractor::ProfileClaimExtractor;
use crate::profile_claim_processor::{ProcessingMetrics, ProfileClaimProcessor};
use crate::profile_claim_verifier::ProfileClaimVerifier;
use crate::profile_image_validator::ProfileImageValidator;
use crate::profile_quality_filter::ProfileQualityFilter;
use crate::storage::ScopedNostrStore;
use nostr::prelude::*;
use reqwest::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, trace, warn};

/// Maximum number of events to process per claim for spam prevention
const MAX_EVENTS_PER_CLAIM: usize = 50;

/// Maximum concurrent requests for profile claim processing
const MAX_CONCURRENT_REQUESTS: usize = 100;

/// Maximum time to wait for a request before timeout
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum retry attempts for processing events
const MAX_RETRY_ATTEMPTS: u32 = 3;

/// Batch size for processing multiple events at once
const BATCH_SIZE: usize = 50;

/// Interval between queue status checks
const QUEUE_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Time between batch processing runs
const BATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum age for events in the delayed queue before they're dropped
const MAX_DELAYED_EVENT_AGE: Duration = Duration::from_secs(300); // 5 minutes

/// Maximum size of the delayed queue
const MAX_DELAYED_QUEUE_SIZE: usize = 10_000;

/// Interval between delayed queue capacity warnings
const CAPACITY_WARNING_INTERVAL: Duration = Duration::from_secs(60); // Log at most once per minute

/// Event to be processed, with metadata for queue management
#[derive(Debug, Clone)]
struct QueuedEvent {
    event: Event,
    retry_count: u32,
    queued_at: Instant,
}

/// Service for processing profile claims from a stream of events
pub struct ProfileValidationPool {
    /// Nostr event store
    store: Arc<ScopedNostrStore>,
    /// HTTP client for fetching external resources
    http_client: Client,
    /// Concurrent request semaphore
    semaphore: Arc<Semaphore>,
    /// Event processing queue
    event_queue: Arc<Mutex<VecDeque<QueuedEvent>>>,
    /// Failed/delayed events queue
    delayed_queue: Arc<Mutex<VecDeque<QueuedEvent>>>,
    /// Metrics tracking
    metrics: Arc<ProcessingMetrics>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
    /// Event receiver
    event_rx: Arc<Mutex<mpsc::Receiver<Event>>>,
    /// Currently processing events (to avoid duplicates)
    processing: Arc<RwLock<HashSet<EventId>>>,
    /// Profile quality filter
    quality_filter: Arc<ProfileQualityFilter>,
    /// Last time capacity warning was logged
    last_capacity_warning: Arc<Mutex<Instant>>,
}

impl ProfileValidationPool {
    /// Creates a new profile validation pool
    pub fn new(
        store: Arc<ScopedNostrStore>,
        event_rx: mpsc::Receiver<Event>,
    ) -> Self {
        let http_client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("NostrProfileValidator/1.0")
            .build()
            .expect("Failed to create HTTP client");

        let quality_filter = Arc::new(ProfileQualityFilter::new());

        Self {
            store,
            http_client,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            event_queue: Arc::new(Mutex::new(VecDeque::new())),
            delayed_queue: Arc::new(Mutex::new(VecDeque::new())),
            metrics: Arc::new(ProcessingMetrics::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            event_rx: Arc::new(Mutex::new(event_rx)),
            processing: Arc::new(RwLock::new(HashSet::new())),
            quality_filter,
            last_capacity_warning: Arc::new(Mutex::new(Instant::now().checked_sub(CAPACITY_WARNING_INTERVAL).unwrap())),
        }
    }

    /// Starts the validation pool
    pub async fn start(self: Arc<Self>) {
        info!("Starting profile validation pool");

        // Start background tasks
        let receiver_handle = self.clone().spawn_receiver();
        let processor_handle = self.clone().spawn_processor();
        let delayed_processor_handle = self.clone().spawn_delayed_processor();
        let metrics_handle = self.clone().spawn_metrics_reporter();

        // Wait for shutdown or task completion
        tokio::select! {
            _ = receiver_handle => warn!("Receiver task ended unexpectedly"),
            _ = processor_handle => warn!("Processor task ended unexpectedly"),
            _ = delayed_processor_handle => warn!("Delayed processor task ended unexpectedly"),
            _ = metrics_handle => warn!("Metrics task ended unexpectedly"),
            _ = self.wait_for_shutdown() => info!("Shutdown signal received"),
        }

        info!("Profile validation pool stopped");
    }

    /// Spawns event receiver task
    fn spawn_receiver(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while !self.shutdown.load(AtomicOrdering::Relaxed) {
                let mut rx = self.event_rx.lock().await;
                
                match rx.recv().await {
                    Some(event) => {
                        // Quick validation before queuing
                        if event.kind == Kind::Metadata {
                            self.metrics.events_received.fetch_add(1, AtomicOrdering::Relaxed);
                            
                            let queued_event = QueuedEvent {
                                event,
                                retry_count: 0,
                                queued_at: Instant::now(),
                            };
                            
                            let mut queue = self.event_queue.lock().await;
                            queue.push_back(queued_event);
                            self.metrics.queue_size.store(queue.len(), AtomicOrdering::Relaxed);
                        }
                    }
                    None => {
                        info!("Event channel closed");
                        break;
                    }
                }
            }
        })
    }

    /// Spawns event processor task
    fn spawn_processor(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut batch_interval = interval(BATCH_INTERVAL);
            
            while !self.shutdown.load(AtomicOrdering::Relaxed) {
                batch_interval.tick().await;
                
                // Process a batch of events
                let batch = {
                    let mut queue = self.event_queue.lock().await;
                    let mut batch = Vec::new();
                    
                    for _ in 0..BATCH_SIZE {
                        if let Some(event) = queue.pop_front() {
                            batch.push(event);
                        } else {
                            break;
                        }
                    }
                    
                    self.metrics.queue_size.store(queue.len(), AtomicOrdering::Relaxed);
                    batch
                };
                
                if batch.is_empty() {
                    continue;
                }
                
                // Process batch concurrently
                let tasks: Vec<_> = batch
                    .into_iter()
                    .map(|queued_event| {
                        let ctx = self.clone();
                        tokio::spawn(async move {
                            ctx.process_event(queued_event).await;
                        })
                    })
                    .collect();
                
                // Wait for all tasks to complete
                for task in tasks {
                    let _ = task.await;
                }
            }
        })
    }

    /// Spawns delayed event reprocessor task
    fn spawn_delayed_processor(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut check_interval = interval(Duration::from_secs(30));
            
            while !self.shutdown.load(AtomicOrdering::Relaxed) {
                check_interval.tick().await;
                
                let ready_events = {
                    let mut queue = self.delayed_queue.lock().await;
                    let mut ready = Vec::new();
                    let now = Instant::now();
                    
                    // Remove old events and collect ready ones
                    queue.retain(|event| {
                        let age = now.duration_since(event.queued_at);
                        
                        if age > MAX_DELAYED_EVENT_AGE {
                            self.metrics.events_expired.fetch_add(1, AtomicOrdering::Relaxed);
                            false // Remove expired events
                        } else if event.retry_count < MAX_RETRY_ATTEMPTS {
                            ready.push(event.clone());
                            false // Remove from delayed queue
                        } else {
                            true // Keep in queue
                        }
                    });
                    
                    self.metrics.delayed_queue_size.store(queue.len(), AtomicOrdering::Relaxed);
                    ready
                };
                
                // Re-queue ready events
                if !ready_events.is_empty() {
                    let mut queue = self.event_queue.lock().await;
                    for event in ready_events {
                        queue.push_back(event);
                    }
                    self.metrics.queue_size.store(queue.len(), AtomicOrdering::Relaxed);
                }
            }
        })
    }

    /// Spawns metrics reporter task
    fn spawn_metrics_reporter(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(60));
            
            while !self.shutdown.load(AtomicOrdering::Relaxed) {
                interval.tick().await;
                
                info!(
                    "Validation pool stats - Received: {}, Processed: {}, Succeeded: {}, Failed: {}, Queue: {}, Delayed: {}, Expired: {}",
                    self.metrics.events_received.load(AtomicOrdering::Relaxed),
                    self.metrics.events_processed.load(AtomicOrdering::Relaxed),
                    self.metrics.successful_validations.load(AtomicOrdering::Relaxed),
                    self.metrics.failed_validations.load(AtomicOrdering::Relaxed),
                    self.metrics.queue_size.load(AtomicOrdering::Relaxed),
                    self.metrics.delayed_queue_size.load(AtomicOrdering::Relaxed),
                    self.metrics.events_expired.load(AtomicOrdering::Relaxed),
                );
            }
        })
    }

    /// Processes a single event
    async fn process_event(&self, mut queued_event: QueuedEvent) {
        let event_id = queued_event.event.id;
        
        // Check if already processing
        {
            let mut processing = self.processing.write().await;
            if !processing.insert(event_id) {
                debug!("Event {} already being processed", event_id);
                return;
            }
        }
        
        // Ensure we remove from processing set when done
        let _guard = scopeguard::guard((), |_| {
            tokio::spawn({
                let processing = self.processing.clone();
                let event_id = event_id;
                async move {
                    processing.write().await.remove(&event_id);
                }
            });
        });
        
        // Apply quality filter
        if !self.quality_filter.should_process(&queued_event.event).await {
            debug!("Event {} filtered out by quality filter", event_id);
            self.metrics.events_processed.fetch_add(1, AtomicOrdering::Relaxed);
            return;
        }
        
        // Process the event
        match self.validate_and_store_profile(queued_event.event.clone()).await {
            Ok(_) => {
                self.metrics.successful_validations.fetch_add(1, AtomicOrdering::Relaxed);
                self.metrics.events_processed.fetch_add(1, AtomicOrdering::Relaxed);
                debug!("Successfully processed event {}", event_id);
            }
            Err(e) => {
                self.metrics.failed_validations.fetch_add(1, AtomicOrdering::Relaxed);
                
                // Determine if we should retry
                queued_event.retry_count += 1;
                if queued_event.retry_count < MAX_RETRY_ATTEMPTS {
                    warn!(
                        "Failed to process event {} (attempt {}): {:?}",
                        event_id, queued_event.retry_count, e
                    );
                    
                    // Add to delayed queue
                    let mut queue = self.delayed_queue.lock().await;
                    
                    if queue.len() >= MAX_DELAYED_QUEUE_SIZE {
                        // Check if we should log a warning
                        let mut last_warning = self.last_capacity_warning.lock().await;
                        let now = Instant::now();
                        
                        if now.duration_since(*last_warning) >= CAPACITY_WARNING_INTERVAL {
                            warn!(
                                "Delayed queue at capacity ({}), dropping new events. {} events expired so far.",
                                MAX_DELAYED_QUEUE_SIZE,
                                self.metrics.events_expired.load(AtomicOrdering::Relaxed)
                            );
                            *last_warning = now;
                        }
                        
                        // Drop this event
                        self.metrics.max_retries_exceeded.fetch_add(1, AtomicOrdering::Relaxed);
                    } else {
                        queue.push_back(queued_event);
                        self.metrics.delayed_queue_size.store(queue.len(), AtomicOrdering::Relaxed);
                    }
                } else {
                    error!(
                        "Failed to process event {} after {} attempts: {:?}",
                        event_id, MAX_RETRY_ATTEMPTS, e
                    );
                    self.metrics.max_retries_exceeded.fetch_add(1, AtomicOrdering::Relaxed);
                }
                
                self.metrics.events_processed.fetch_add(1, AtomicOrdering::Relaxed);
            }
        }
    }

    /// Validates and stores a profile from an event
    async fn validate_and_store_profile(&self, event: Event) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Extract profile claims
        let extractor = ProfileClaimExtractor;
        let claims = extractor.extract(&event)?;
        
        if claims.is_empty() {
            return Ok(()); // No claims to process
        }
        
        // Create validation components
        let verifier = ProfileClaimVerifier::new(self.http_client.clone());
        let validator = ProfileImageValidator::new(self.http_client.clone());
        let processor = ProfileClaimProcessor::new(
            Arc::new(verifier),
            Arc::new(validator),
            self.store.clone(),
            self.semaphore.clone(),
        );
        
        // Process all claims
        let results = processor.process_claims(&event, claims).await;
        
        // Check if any succeeded
        if results.iter().any(|r| r.is_ok()) {
            Ok(())
        } else {
            Err("All profile claims failed validation".into())
        }
    }

    /// Waits for shutdown signal
    async fn wait_for_shutdown(&self) {
        while !self.shutdown.load(AtomicOrdering::Relaxed) {
            sleep(Duration::from_secs(1)).await;
        }
    }

    /// Initiates shutdown
    pub fn shutdown(&self) {
        info!("Initiating profile validation pool shutdown");
        self.shutdown.store(true, AtomicOrdering::Relaxed);
    }

    /// Gets current metrics
    pub fn metrics(&self) -> ProcessingMetrics {
        ProcessingMetrics {
            events_received: AtomicU64::new(self.metrics.events_received.load(AtomicOrdering::Relaxed)),
            events_processed: AtomicU64::new(self.metrics.events_processed.load(AtomicOrdering::Relaxed)),
            successful_validations: AtomicU64::new(self.metrics.successful_validations.load(AtomicOrdering::Relaxed)),
            failed_validations: AtomicU64::new(self.metrics.failed_validations.load(AtomicOrdering::Relaxed)),
            max_retries_exceeded: AtomicU64::new(self.metrics.max_retries_exceeded.load(AtomicOrdering::Relaxed)),
            queue_size: AtomicUsize::new(self.metrics.queue_size.load(AtomicOrdering::Relaxed)),
            delayed_queue_size: AtomicUsize::new(self.metrics.delayed_queue_size.load(AtomicOrdering::Relaxed)),
            events_expired: AtomicU64::new(self.metrics.events_expired.load(AtomicOrdering::Relaxed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::create_test_store;

    #[tokio::test]
    async fn test_pool_creation() {
        let store = create_test_store().await;
        let (tx, rx) = mpsc::channel(100);
        let pool = Arc::new(ProfileValidationPool::new(store, rx));
        
        assert_eq!(pool.metrics.events_received.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(pool.metrics.events_processed.load(AtomicOrdering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_event_processing() {
        let store = create_test_store().await;
        let (tx, rx) = mpsc::channel(100);
        let pool = Arc::new(ProfileValidationPool::new(store, rx));
        
        // Create a test event
        let event = Event::new(
            Kind::Metadata,
            "{}",
            &Keys::generate(),
        );
        
        // Send event
        tx.send(event).await.unwrap();
        
        // Start pool in background
        let pool_handle = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.start().await;
            }
        });
        
        // Wait a bit for processing
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Check metrics
        assert!(pool.metrics.events_received.load(AtomicOrdering::Relaxed) > 0);
        
        // Shutdown
        pool.shutdown();
        drop(tx);
        
        // Wait for pool to finish
        tokio::time::timeout(Duration::from_secs(5), pool_handle)
            .await
            .expect("Pool shutdown timeout")
            .expect("Pool task failed");
    }
}