use crate::profile_quality_filter::ProfileQualityFilter;
use crate::profile_validator::{ProfileValidator, ProfileValidatorMetricsSnapshot};
use nostr_relay_builder::{CryptoWorker, RelayDatabase};
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

fn create_test_setup() -> (Arc<ProfileValidator>, Arc<RelayDatabase>, CancellationToken) {
    let temp_dir = TempDir::new().unwrap();
    let keys = Keys::generate();
    let cancellation_token = CancellationToken::new();
    let task_tracker = TaskTracker::new();
    let crypto_sender = CryptoWorker::spawn(Arc::new(keys.clone()), &task_tracker);
    let db = Arc::new(RelayDatabase::new(temp_dir.path(), crypto_sender).unwrap());
    let filter = Arc::new(ProfileQualityFilter::new(db.clone()));

    // Create a gossip client for testing
    let keys = Keys::generate();
    let gossip_client = Arc::new(Client::new(keys));

    let validator = Arc::new(ProfileValidator::new(filter, db.clone(), gossip_client));

    (validator, db, cancellation_token)
}

fn create_test_profile_event(name: &str, about: &str, picture: &str) -> Event {
    let content = format!(
        r#"{{"name":"{}","about":"{}","picture":"{}"}}"#,
        name, about, picture
    );
    let keys = Keys::generate();
    EventBuilder::new(Kind::Metadata, content)
        .sign_with_keys(&keys)
        .unwrap()
}

#[tokio::test]
async fn test_validator_initialization() {
    let (validator, _, _) = create_test_setup();

    let metrics = validator.metrics().await;
    assert_eq!(metrics.current_processing, 0);
    assert_eq!(metrics.current_delayed_retry_queue, 0);
    assert_eq!(metrics.total_processed, 0);
    assert_eq!(metrics.total_accepted, 0);
    assert_eq!(metrics.total_rejected, 0);
}

#[tokio::test]
async fn test_process_profiles() {
    let (validator, _, _) = create_test_setup();

    let events = vec![
        create_test_profile_event("User1", "About user 1", "https://example.com/1.jpg"),
        create_test_profile_event("User2", "About user 2", "https://example.com/2.jpg"),
        create_test_profile_event("User3", "About user 3", "https://example.com/3.jpg"),
    ];

    validator
        .process_profiles(events, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Give processing time to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    let metrics = validator.metrics().await;
    assert_eq!(metrics.total_processed, 3);
}

#[tokio::test]
async fn test_rate_limited_retry() {
    let (validator, _, _) = create_test_setup();

    // Create event with known rate-limited domain
    let event = create_test_profile_event(
        "Test User",
        "Test about",
        "https://placeholder.com/image.jpg", // This will be rejected as placeholder
    );

    validator
        .process_profiles(vec![event], nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Give processing time to complete
    tokio::time::sleep(Duration::from_millis(200)).await;

    let metrics = validator.metrics().await;
    assert!(metrics.total_processed > 0);
    assert_eq!(metrics.total_accepted, 0); // Should be rejected
}

#[tokio::test]
async fn test_delayed_retry_processing() {
    let (validator, _, _) = create_test_setup();

    // Process an event that will be rate limited
    let event = create_test_profile_event("User", "About", "https://placeholder.com/test.jpg");

    validator
        .process_profiles(vec![event], nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Check that it's in the delayed queue
    let metrics = validator.metrics().await;
    assert_eq!(metrics.current_delayed_retry_queue, 0); // Not yet in queue due to async processing

    // Process ready retries (should be none immediately)
    let processed = validator.process_ready_retries().await;
    assert_eq!(processed, 0);
}

#[tokio::test]
async fn test_cleanup() {
    let (validator, _, _) = create_test_setup();

    // Run cleanup (should work even with empty queues)
    validator.cleanup().await;

    let metrics = validator.metrics().await;
    assert_eq!(metrics.current_delayed_retry_queue, 0);
}

#[tokio::test]
async fn test_metrics_accuracy() {
    let (validator, _, _) = create_test_setup();

    let initial_metrics = validator.metrics().await;

    // Submit profiles that will be rejected (no picture)
    let events = vec![
        create_test_profile_event("User1", "About", ""),
        create_test_profile_event("User2", "About", ""),
    ];

    validator
        .process_profiles(events, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(200)).await;

    let final_metrics = validator.metrics().await;
    assert!(final_metrics.total_processed > initial_metrics.total_processed);
    assert_eq!(final_metrics.total_accepted, initial_metrics.total_accepted); // All rejected
    assert!(final_metrics.total_rejected > initial_metrics.total_rejected);
}

#[test]
fn test_metrics_snapshot() {
    let snapshot = ProfileValidatorMetricsSnapshot {
        current_processing: 1,
        current_delayed_retry_queue: 5,
        total_processed: 100,
        total_accepted: 80,
        total_rejected: 20,
        total_failed: 0,
        total_rate_limited: 15,
        total_max_retries_exceeded: 2,
        top_rate_limited_domains: vec![("example.com".to_string(), 10)],
    };

    assert_eq!(snapshot.current_processing, 1);
    assert_eq!(snapshot.total_processed, 100);
    assert_eq!(snapshot.total_accepted + snapshot.total_rejected, 100);
}
