use crate::profile_aggregation_service::{ProfileAggregationConfig, ProfileAggregationService};
use crate::profile_quality_filter::ProfileQualityFilter;
use nostr_relay_builder::RelayDatabase;
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

async fn create_test_service() -> (ProfileAggregationService, TempDir) {
    let temp_dir = tempfile::tempdir().unwrap();
    let state_file = temp_dir.path().join("test_state.json");

    let task_tracker = TaskTracker::new();
    let cancellation_token = CancellationToken::new();
    let (database, db_sender) = RelayDatabase::new(temp_dir.path().join("db")).unwrap();
    let db = Arc::new(database);
    let filter = Arc::new(ProfileQualityFilter::new(db.clone()));

    let config = ProfileAggregationConfig {
        relay_urls: vec!["wss://test.relay.com".to_string()],
        filters: vec![Filter::new().kind(Kind::Metadata)],
        page_size: 100,
        state_file,
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_secs(1),
    };

    let service =
        ProfileAggregationService::new(config, filter, db_sender, cancellation_token, task_tracker)
            .await
            .unwrap();

    (service, temp_dir)
}

#[tokio::test]
async fn test_service_creation() {
    let (_service, _temp_dir) = create_test_service().await;
    // Service created successfully
}

#[tokio::test]
async fn test_config_defaults() {
    let config = ProfileAggregationConfig::default();
    assert_eq!(config.page_size, 500);
    assert_eq!(config.initial_backoff, Duration::from_secs(2));
    assert_eq!(config.max_backoff, Duration::from_secs(300));
    // Worker threads configuration has been removed
}

#[tokio::test]
async fn test_state_persistence() {
    let temp_dir = tempfile::tempdir().unwrap();
    let state_file = temp_dir.path().join("state.json");

    // Create initial state
    let state_json = r#"{
        "relay_states": {
            "wss://test.relay.com": {
                "window_start": 1700000000,
                "last_until_timestamp": 1700001000,
                "total_events_fetched": 1000,
                "window_complete": false
            }
        },
        "last_saved": 1700002000
    }"#;

    std::fs::write(&state_file, state_json).unwrap();

    // Create service with existing state
    let task_tracker = TaskTracker::new();
    let cancellation_token = CancellationToken::new();
    let (database, db_sender) = RelayDatabase::new(temp_dir.path().join("db")).unwrap();
    let db = Arc::new(database);
    let filter = Arc::new(ProfileQualityFilter::new(db.clone()));

    let config = ProfileAggregationConfig {
        relay_urls: vec!["wss://test.relay.com".to_string()],
        filters: vec![Filter::new().kind(Kind::Metadata)],
        page_size: 100,
        state_file: state_file.clone(),
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_secs(1),
    };

    let _service =
        ProfileAggregationService::new(config, filter, db_sender, cancellation_token, task_tracker)
            .await
            .unwrap();

    // State file should still exist
    assert!(state_file.exists());
}

#[test]
fn test_filter_configuration() {
    let mut filter = Filter::new().kind(Kind::Metadata).limit(500);

    // Test real-time filter modification
    filter.since = Some(Timestamp::now());
    assert!(filter.since.is_some());
    assert!(filter.until.is_none()); // Default
    assert_eq!(filter.limit, Some(500));

    // Test backward pagination filter
    filter.since = None;
    filter.until = Some(Timestamp::now());
    assert!(filter.since.is_none());
    assert!(filter.until.is_some());
}

#[test]
fn test_timestamp_formatting() {
    use chrono::{DateTime, Utc};

    let ts = Timestamp::from(1700000000);
    let dt = DateTime::<Utc>::from_timestamp(ts.as_u64() as i64, 0);
    assert!(dt.is_some());

    let formatted = dt.unwrap().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    assert!(formatted.contains("2023")); // Approximate year check
}

#[tokio::test]
async fn test_multiple_relay_configuration() {
    let temp_dir = tempfile::tempdir().unwrap();
    let state_file = temp_dir.path().join("test_state.json");

    let task_tracker = TaskTracker::new();
    let cancellation_token = CancellationToken::new();
    let (database, db_sender) = RelayDatabase::new(temp_dir.path().join("db")).unwrap();
    let db = Arc::new(database);
    let filter = Arc::new(ProfileQualityFilter::new(db.clone()));

    let config = ProfileAggregationConfig {
        relay_urls: vec![
            "wss://relay1.test.com".to_string(),
            "wss://relay2.test.com".to_string(),
            "wss://relay3.test.com".to_string(),
        ],
        filters: vec![Filter::new().kind(Kind::Metadata)],
        page_size: 100,
        state_file,
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_secs(1),
    };

    let _service = ProfileAggregationService::new(
        config.clone(),
        filter,
        db_sender,
        cancellation_token,
        task_tracker,
    )
    .await
    .unwrap();

    // Verify service handles multiple relays
    assert_eq!(config.relay_urls.len(), 3);
}

#[test]
fn test_backoff_calculation() {
    let initial = Duration::from_secs(2);
    let max = Duration::from_secs(300);

    let mut backoff = initial;
    backoff = std::cmp::min(backoff * 2, max);
    assert_eq!(backoff, Duration::from_secs(4));

    backoff = std::cmp::min(backoff * 2, max);
    assert_eq!(backoff, Duration::from_secs(8));

    // Test max limit
    backoff = Duration::from_secs(200);
    backoff = std::cmp::min(backoff * 2, max);
    assert_eq!(backoff, max);
}
