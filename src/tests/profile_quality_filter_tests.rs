use crate::profile_quality_filter::{NostrProfileData, ProfileQualityFilter};
use nostr_sdk::prelude::*;
use relay_builder::RelayDatabase;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_database() -> Arc<RelayDatabase> {
    let temp_dir = TempDir::new().unwrap();
    let database = RelayDatabase::new(temp_dir.path()).unwrap();
    Arc::new(database)
}

fn create_test_event(content: &str) -> Event {
    let keys = Keys::generate();
    EventBuilder::new(Kind::Metadata, content)
        .sign_with_keys(&keys)
        .unwrap()
}

#[tokio::test]
async fn test_profile_without_name_rejected() {
    let db = create_test_database();
    let filter = ProfileQualityFilter::new(db);

    let profile_json = r#"{
        "about": "Test profile",
        "picture": "https://example.com/pic.jpg"
    }"#;

    let event = create_test_event(profile_json);
    let result = filter
        .apply_filter(event, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    assert!(result.is_empty(), "Profile without name should be rejected");
}

#[tokio::test]
async fn test_profile_without_about_rejected() {
    let db = create_test_database();
    let filter = ProfileQualityFilter::new(db);

    let profile_json = r#"{
        "name": "Test User",
        "picture": "https://example.com/pic.jpg"
    }"#;

    let event = create_test_event(profile_json);
    let result = filter
        .apply_filter(event, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    assert!(
        result.is_empty(),
        "Profile without about should be rejected"
    );
}

#[tokio::test]
async fn test_profile_without_picture_rejected() {
    let db = create_test_database();
    let filter = ProfileQualityFilter::new(db);

    let profile_json = r#"{
        "name": "Test User",
        "about": "Test profile description"
    }"#;

    let event = create_test_event(profile_json);
    let result = filter
        .apply_filter(event, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    assert!(
        result.is_empty(),
        "Profile without picture should be rejected"
    );
}

#[tokio::test]
async fn test_mostr_profile_rejected() {
    let db = create_test_database();
    let filter = ProfileQualityFilter::new(db);

    let profile_json = r#"{
        "name": "Test User",
        "about": "Test profile",
        "picture": "https://example.com/pic.jpg",
        "nip05": "user@mostr.pub"
    }"#;

    let event = create_test_event(profile_json);
    let result = filter
        .apply_filter(event, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    assert!(result.is_empty(), "Mostr profile should be rejected");
}

#[tokio::test]
async fn test_profile_with_fields_rejected() {
    let db = create_test_database();
    let filter = ProfileQualityFilter::new(db);

    let profile_json = r#"{
        "name": "Test User",
        "about": "Test profile",
        "picture": "https://example.com/pic.jpg",
        "fields": [{"name": "Website", "value": "https://example.com"}]
    }"#;

    let event = create_test_event(profile_json);
    let result = filter
        .apply_filter(event, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    assert!(result.is_empty(), "Profile with fields should be rejected");
}

#[tokio::test]
async fn test_placeholder_image_rejected() {
    let db = create_test_database();
    let filter = ProfileQualityFilter::new(db);

    let profile_json = r#"{
        "name": "Test User",
        "about": "Test profile",
        "picture": "https://placeholder.com/image.jpg"
    }"#;

    let event = create_test_event(profile_json);
    let result = filter
        .apply_filter(event, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    assert!(
        result.is_empty(),
        "Profile with placeholder image should be rejected"
    );
}

#[tokio::test]
async fn test_relay_list_without_metadata_rejected() {
    let db = create_test_database();
    let filter = ProfileQualityFilter::new(db);

    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(10002), "")
        .sign_with_keys(&keys)
        .unwrap();

    let result = filter
        .apply_filter(event, nostr_lmdb::Scope::Default)
        .await
        .unwrap();

    assert!(
        result.is_empty(),
        "Relay list without existing metadata should be rejected"
    );
}

#[test]
fn test_profile_data_deserialization() {
    let json = r#"{
        "name": "Test User",
        "display_name": "Test Display",
        "about": "Test about",
        "picture": "https://example.com/pic.jpg",
        "banner": "https://example.com/banner.jpg",
        "nip05": "test@example.com",
        "lud16": "test@getalby.com",
        "website": "https://example.com"
    }"#;

    let profile: NostrProfileData = serde_json::from_str(json).unwrap();
    assert_eq!(profile.name, Some("Test User".to_string()));
    assert_eq!(profile.display_name, Some("Test Display".to_string()));
    assert_eq!(profile.about, Some("Test about".to_string()));
    assert_eq!(
        profile.picture,
        Some("https://example.com/pic.jpg".to_string())
    );
}

#[test]
fn test_profile_data_partial_deserialization() {
    let json = r#"{
        "name": "Test User",
        "extra_field": "ignored"
    }"#;

    let profile: NostrProfileData = serde_json::from_str(json).unwrap();
    assert_eq!(profile.name, Some("Test User".to_string()));
    assert!(profile.about.is_none());
    assert!(profile.picture.is_none());
}
