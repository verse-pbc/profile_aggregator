use crate::profile_quality_filter::ProfileQualityFilter;
use crate::profile_validation_pool::{ProfileValidationPool, ProfileValidatorMetricsSnapshot};
use nostr_relay_builder::{CryptoWorker, RelayDatabase};
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn create_test_setup() -> (
    Arc<ProfileValidationPool>,
    Arc<RelayDatabase>,
    CancellationToken,
) {
    let temp_dir = TempDir::new().unwrap();
    let keys = Keys::generate();
    let cancellation_token = CancellationToken::new();
    let crypto_worker = Arc::new(CryptoWorker::new(
        Arc::new(keys.clone()),
        cancellation_token.clone(),
    ));
    let db = Arc::new(RelayDatabase::new(temp_dir.path(), crypto_worker).unwrap());
    let filter = Arc::new(ProfileQualityFilter::new(db.clone()));

    // Create a gossip client for testing
    let keys = Keys::generate();
    let gossip_client = Arc::new(Client::new(keys));

    let pool = Arc::new(ProfileValidationPool::new(
        4, // 4 workers for testing
        filter,
        db.clone(),
        cancellation_token.clone(),
        gossip_client,
    ));

    (pool, db, cancellation_token)
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
async fn test_pool_initialization() {
    let (pool, _, _) = create_test_setup();

    let metrics = pool.metrics().await;
    assert_eq!(metrics.current_queued, 0);
    assert_eq!(metrics.total_processed, 0);
    assert_eq!(metrics.total_accepted, 0);
    assert_eq!(metrics.total_rejected, 0);
}

#[tokio::test]
async fn test_submit_profiles() {
    let (pool, _, _) = create_test_setup();

    let events = vec![
        create_test_profile_event("User1", "About user 1", "https://example.com/1.jpg"),
        create_test_profile_event("User2", "About user 2", "https://example.com/2.jpg"),
        create_test_profile_event("User3", "About user 3", "https://example.com/3.jpg"),
    ];

    pool.submit_profiles(events, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Give workers time to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    let metrics = pool.metrics().await;
    assert!(metrics.current_queued > 0 || metrics.total_processed > 0);
}

#[tokio::test]
async fn test_rate_limited_retry() {
    let (pool, _, _) = create_test_setup();

    // Create event with known rate-limited domain
    let event = create_test_profile_event(
        "Test User",
        "Test about",
        "https://placeholder.com/image.jpg", // This will be rejected as placeholder
    );

    pool.submit_profiles(vec![event], nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Give workers time to process
    tokio::time::sleep(Duration::from_millis(200)).await;

    let metrics = pool.metrics().await;
    assert!(metrics.total_processed > 0);
    assert_eq!(metrics.total_accepted, 0); // Should be rejected
}

#[tokio::test]
async fn test_cancellation() {
    let (pool, _, cancellation_token) = create_test_setup();

    // Submit many events
    let mut events = Vec::new();
    for i in 0..100 {
        events.push(create_test_profile_event(
            &format!("User{}", i),
            &format!("About user {}", i),
            &format!("https://example.com/{}.jpg", i),
        ));
    }

    pool.submit_profiles(events, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Cancel immediately
    cancellation_token.cancel();

    // Give workers time to stop
    tokio::time::sleep(Duration::from_millis(100)).await;

    let metrics = pool.metrics().await;
    // Should have processed some but not all due to cancellation
    assert!(metrics.total_processed < 100);
}

#[tokio::test]
async fn test_metrics_accuracy() {
    let (pool, _, _) = create_test_setup();

    let initial_metrics = pool.metrics().await;

    // Submit profiles that will be rejected (no picture)
    let events = vec![
        create_test_profile_event("User1", "About", ""),
        create_test_profile_event("User2", "About", ""),
    ];

    pool.submit_profiles(events, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(200)).await;

    let final_metrics = pool.metrics().await;
    assert!(final_metrics.total_processed > initial_metrics.total_processed);
    assert_eq!(final_metrics.total_accepted, initial_metrics.total_accepted); // All rejected
    assert!(final_metrics.total_rejected > initial_metrics.total_rejected);
}

#[test]
fn test_metrics_snapshot() {
    let snapshot = ProfileValidatorMetricsSnapshot {
        current_queued: 10,
        current_delayed_retry_queue: 5,
        total_processed: 100,
        total_accepted: 80,
        total_rejected: 20,
        total_failed: 0,
        total_rate_limited: 15,
        total_max_retries_exceeded: 2,
        top_rate_limited_domains: vec![("example.com".to_string(), 10)],
    };

    assert_eq!(snapshot.current_queued, 10);
    assert_eq!(snapshot.total_processed, 100);
    assert_eq!(snapshot.total_accepted + snapshot.total_rejected, 100);
}
