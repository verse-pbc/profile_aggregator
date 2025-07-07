//! Avatar sync functionality for triggering CloudFlare R2 cache updates
//! This module provides fire-and-forget HTTP requests to the CloudFlare worker

use nostr_sdk::prelude::*;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Configuration for avatar sync
#[derive(Clone, Debug)]
pub struct AvatarSyncConfig {
    /// Base URL for the CloudFlare worker
    pub base_url: String,
    /// Request timeout
    pub timeout: Duration,
}

impl Default for AvatarSyncConfig {
    fn default() -> Self {
        Self {
            base_url: "https://face.yestr.social".to_string(),
            timeout: Duration::from_secs(30),
        }
    }
}

/// Avatar sync client for triggering CloudFlare cache updates
#[derive(Clone)]
pub struct AvatarSyncClient {
    client: Client,
    config: AvatarSyncConfig,
}

impl AvatarSyncClient {
    /// Create a new avatar sync client
    pub fn new(config: AvatarSyncConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = Client::builder().timeout(config.timeout).build()?;

        Ok(Self { client, config })
    }

    /// Create with default configuration
    pub fn new_default() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new(AvatarSyncConfig::default())
    }

    /// Trigger avatar sync for a pubkey (fire and forget)
    pub fn trigger_sync(&self, pubkey: PublicKey) {
        let url = format!("{}/avatar/{}", self.config.base_url, pubkey.to_hex());
        let client = self.client.clone();

        // Spawn a task to make the request without waiting for it
        tokio::spawn(async move {
            debug!("Triggering avatar sync for {}", pubkey.to_hex());

            match client.get(&url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        debug!("Avatar sync triggered successfully for {}", pubkey.to_hex());
                    } else {
                        debug!(
                            "Avatar sync returned {} for {}",
                            response.status(),
                            pubkey.to_hex()
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to trigger avatar sync for {}: {}",
                        pubkey.to_hex(),
                        e
                    );
                }
            }
        });
    }

    /// Trigger sync for a kind 0 event if it has a picture
    pub fn trigger_sync_for_event(&self, event: &Event) {
        // Only process kind 0 events
        if event.kind != Kind::Metadata {
            return;
        }

        // Try to parse the metadata to check if it has a picture
        if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&event.content) {
            if metadata.get("picture").and_then(|p| p.as_str()).is_some() {
                self.trigger_sync(event.pubkey);
            }
        }
    }
}

/// Global avatar sync client instance (optional - can be used for convenience)
static AVATAR_SYNC_CLIENT: std::sync::OnceLock<Arc<AvatarSyncClient>> = std::sync::OnceLock::new();

/// Initialize the global avatar sync client
pub fn init_global_client(
    config: AvatarSyncConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Arc::new(AvatarSyncClient::new(config)?);
    AVATAR_SYNC_CLIENT
        .set(client)
        .map_err(|_| "Avatar sync client already initialized")?;
    Ok(())
}

/// Get the global avatar sync client
pub fn get_global_client() -> Option<Arc<AvatarSyncClient>> {
    AVATAR_SYNC_CLIENT.get().cloned()
}

/// Convenience function to trigger sync using the global client
pub fn trigger_sync(pubkey: PublicKey) {
    if let Some(client) = get_global_client() {
        client.trigger_sync(pubkey);
    }
}

/// Convenience function to trigger sync for an event using the global client
pub fn trigger_sync_for_event(event: &Event) {
    if let Some(client) = get_global_client() {
        client.trigger_sync_for_event(event);
    }
}
