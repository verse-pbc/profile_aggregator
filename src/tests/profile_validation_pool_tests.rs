use crate::profile_quality_filter::ProfileQualityFilter;
use crate::profile_validation_pool::{ProfileValidationPool, ProfileValidatorMetricsSnapshot};
use nostr_relay_builder::{CryptoWorker, RelayDatabase};
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn create_test_setup() -> (Arc<ProfileValidationPool>, Arc<RelayDatabase>, CancellationToken) {
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
    
    let metrics = pool.metrics();
    assert_eq!(metrics.queued_operations, 0);
    assert_eq!(metrics.processed_profiles, 0);
    assert_eq!(metrics.accepted_profiles, 0);
    assert_eq!(metrics.rejected_profiles, 0);
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
    
    let metrics = pool.metrics();
    assert!(metrics.queued_operations > 0 || metrics.processed_profiles > 0);
}

#[tokio::test]
async fn test_rate_limited_retry() {
    let (pool, _, _) = create_test_setup();
    
    // Create event with known rate-limited domain
    let event = create_test_profile_event(
        "Test User",
        "Test about",
        "https://placeholder.com/image.jpg" // This will be rejected as placeholder
    );
    
    pool.submit_profiles(vec![event], nostr_lmdb::Scope::Default)
        .await
        .unwrap();
    
    // Give workers time to process
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    let metrics = pool.metrics();
    assert!(metrics.processed_profiles > 0);
    assert_eq!(metrics.accepted_profiles, 0); // Should be rejected
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
    
    let metrics = pool.metrics();
    // Should have processed some but not all due to cancellation
    assert!(metrics.processed_profiles < 100);
}

#[tokio::test]
async fn test_metrics_accuracy() {
    let (pool, _, _) = create_test_setup();
    
    let initial_metrics = pool.metrics();
    
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
    
    let final_metrics = pool.metrics();
    assert!(final_metrics.processed_profiles > initial_metrics.processed_profiles);
    assert_eq!(final_metrics.accepted_profiles, initial_metrics.accepted_profiles); // All rejected
    assert!(final_metrics.rejected_profiles > initial_metrics.rejected_profiles);
}

#[test]
fn test_metrics_snapshot() {
    let snapshot = ProfileValidatorMetricsSnapshot {
        queued_operations: 10,
        delayed_operations: 5,
        processed_profiles: 100,
        accepted_profiles: 80,
        rejected_profiles: 20,
        failed_operations: 0,
        rate_limited_retries: 15,
        max_retries_exceeded: 2,
    };
    
    assert_eq!(snapshot.queued_operations, 10);
    assert_eq!(snapshot.processed_profiles, 100);
    assert_eq!(snapshot.accepted_profiles + snapshot.rejected_profiles, 100);
}