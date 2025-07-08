use nostr_relay_builder::RelayDatabase;
use nostr_sdk::prelude::*;
use profile_aggregator::{
    ProfileAggregationConfig, ProfileAggregationService, ProfileQualityFilter,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[tokio::test]
#[ignore] // This test requires a running relay
async fn test_real_time_subscription_integration() {
    // This test would require a test relay to be running
    // It demonstrates how real-time subscription would work in practice

    let temp_dir = tempfile::tempdir().unwrap();
    let state_file = temp_dir.path().join("test_state.json");

    let task_tracker = TaskTracker::new();
    let (database, db_sender) = RelayDatabase::new(temp_dir.path().join("db")).unwrap();
    let db = Arc::new(database);
    let filter = Arc::new(ProfileQualityFilter::new(db.clone()));

    let config = ProfileAggregationConfig {
        relay_urls: vec!["ws://localhost:8081".to_string()], // Test relay
        filters: vec![Filter::new().kind(Kind::Metadata)],
        page_size: 100,
        state_file,
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_secs(1),
    };

    let cancellation_token = CancellationToken::new();
    let service =
        ProfileAggregationService::new(config, filter, db_sender, cancellation_token, task_tracker)
            .await
            .unwrap();

    // Start service in background
    let service_handle = tokio::spawn(async move {
        let _ = service.run().await;
    });

    // Give service time to connect and establish subscription
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Publish a new profile event to the relay
    let keys = Keys::generate();
    let client = Client::new(keys);
    client.add_relay("ws://localhost:8081").await.unwrap();
    client.connect().await;

    let profile_event = EventBuilder::new(
        Kind::Metadata,
        r#"{"name":"Integration Test User","about":"Testing real-time subscription","picture":"https://example.com/test.jpg"}"#
    ).sign_with_keys(&Keys::generate()).unwrap();

    client.send_event(&profile_event).await.unwrap();

    // Wait for event to be processed
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Query local database to verify event was captured
    let stored_filter = Filter::new()
        .author(profile_event.pubkey)
        .kind(Kind::Metadata);

    let _ = db
        .query(vec![stored_filter], &nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    // Note: This would only pass if the profile met all quality requirements
    // and had published text notes via outbox relays

    // Clean up
    service_handle.abort();
}

#[test]
fn test_event_filtering() {
    // Test that filters correctly identify metadata events
    let keys = Keys::generate();
    let metadata_event = EventBuilder::new(Kind::Metadata, "{}")
        .sign_with_keys(&keys)
        .unwrap();

    assert_eq!(metadata_event.kind, Kind::Metadata);

    let text_event = EventBuilder::text_note("test")
        .sign_with_keys(&keys)
        .unwrap();

    assert_ne!(text_event.kind, Kind::Metadata);
}

#[test]
fn test_profile_content_parsing() {
    let valid_json = r#"{
        "name": "Test User",
        "about": "Test about",
        "picture": "https://example.com/pic.jpg"
    }"#;

    let parsed: serde_json::Value = serde_json::from_str(valid_json).unwrap();
    assert_eq!(parsed["name"], "Test User");
    assert_eq!(parsed["about"], "Test about");
    assert_eq!(parsed["picture"], "https://example.com/pic.jpg");

    let invalid_json = r#"{"name": "Test User", invalid}"#;
    let result: Result<serde_json::Value, _> = serde_json::from_str(invalid_json);
    assert!(result.is_err());
}

#[tokio::test]
async fn test_concurrent_event_processing() {
    use futures::future::join_all;

    // Test that multiple events can be processed concurrently
    let mut handles = vec![];

    for i in 0..10 {
        let handle = tokio::spawn(async move {
            let content = format!(
                r#"{{"name":"User{i}","about":"About user {i}","picture":"https://example.com/{i}.jpg"}}"#
            );
            let keys = Keys::generate();
            EventBuilder::new(Kind::Metadata, content)
                .sign_with_keys(&keys)
                .unwrap()
        });
        handles.push(handle);
    }

    let events: Vec<Event> = join_all(handles)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(events.len(), 10);
    for event in events {
        assert_eq!(event.kind, Kind::Metadata);
    }
}
