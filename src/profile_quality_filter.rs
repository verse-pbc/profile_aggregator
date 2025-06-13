//! Profile quality filter implementation

use crate::profile_image_validator::{ProfileImageValidator, ImageInfo};
use async_trait::async_trait;
use nostr_relay_builder::{Error, EventContext, EventProcessor, RelayDatabase, StoreCommand};
use nostr_sdk::prelude::*;
use std::fmt;
use std::sync::Arc;
use tracing::{debug, warn};

/// Profile quality filter for validating user profiles
#[derive(Clone)]
pub struct ProfileQualityFilter {
    image_validator: Arc<ProfileImageValidator>,
    database: Arc<RelayDatabase>,
}

impl fmt::Debug for ProfileQualityFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileQualityFilter")
            .field("image_validator", &"ProfileImageValidator")
            .finish()
    }
}

impl ProfileQualityFilter {
    pub fn new(database: Arc<RelayDatabase>) -> Self {
        Self {
            // Single image validator - parallelism is now at the profile validation level
            image_validator: Arc::new(ProfileImageValidator::new(300, 600, true)),
            database,
        }
    }

    /// Check if a metadata event exists for the given author
    async fn check_existing_metadata(
        &self,
        author: PublicKey,
        subdomain: &nostr_lmdb::Scope,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let filter = Filter::new().author(author).kind(Kind::Metadata);

        let events = self
            .database
            .query(vec![filter], subdomain)
            .await
            .map_err(|e| format!("Failed to query database: {}", e))?;

        Ok(!events.is_empty())
    }

    /// Get delete command if existing pubkey should be removed
    async fn get_delete_if_exists(
        &self,
        author: PublicKey,
        subdomain: &nostr_lmdb::Scope,
    ) -> Result<Vec<StoreCommand>, Box<dyn std::error::Error + Send + Sync>> {
        if self.check_existing_metadata(author, subdomain).await? {
            let cleanup_filter = Filter::new().author(author);
            Ok(vec![StoreCommand::DeleteEvents(
                cleanup_filter,
                subdomain.clone(),
            )])
        } else {
            Ok(vec![])
        }
    }


    /// Apply the quality filter to an event with gossip client for outbox verification
    pub async fn apply_filter_with_gossip(
        &self,
        event: Event,
        subdomain: nostr_lmdb::Scope,
        gossip_client: &Client,
    ) -> Result<Vec<StoreCommand>, Box<dyn std::error::Error + Send + Sync>> {
        self.apply_filter_internal(event, subdomain, Some(gossip_client)).await
    }

    /// Apply the quality filter to an event (without outbox verification)
    pub async fn apply_filter(
        &self,
        event: Event,
        subdomain: nostr_lmdb::Scope,
    ) -> Result<Vec<StoreCommand>, Box<dyn std::error::Error + Send + Sync>> {
        self.apply_filter_internal(event, subdomain, None).await
    }
    
    /// Internal filter application logic
    async fn apply_filter_internal(
        &self,
        event: Event,
        subdomain: nostr_lmdb::Scope,
        gossip_client: Option<&Client>,
    ) -> Result<Vec<StoreCommand>, Box<dyn std::error::Error + Send + Sync>> {
        // Handle relay list events (kind 10002) - only accept if the author has metadata
        if event.kind == Kind::Custom(10002) {
            // Check if this pubkey has existing metadata in the database
            if self
                .check_existing_metadata(event.pubkey, &subdomain)
                .await?
            {
                return Ok(vec![StoreCommand::SaveSignedEvent(
                    Box::new(event),
                    subdomain,
                )]);
            } else {
                debug!("Rejecting relay list - no metadata exists for pubkey");
                return Ok(vec![]);
            }
        }

        // Only accept metadata events for quality filtering
        if event.kind != Kind::Metadata {
            return Ok(vec![]);
        }

        // Skip Mastodon/ActivityPub bridges
        let raw_tags = serde_json::to_string(&event.tags).unwrap_or_default();
        if raw_tags.contains("\"proxy\"") || raw_tags.contains("\"Mostr\"") {
            debug!("Skipping bridged/Mostr account");
            return self.get_delete_if_exists(event.pubkey, &subdomain).await;
        }

        // Parse metadata
        let metadata: serde_json::Value = match serde_json::from_str(&event.content) {
            Ok(m) => m,
            Err(e) => {
                debug!("Invalid metadata JSON: {}", e);
                return self.get_delete_if_exists(event.pubkey, &subdomain).await;
            }
        };

        // Check for mostr.pub NIP-05
        if let Some(nip05) = metadata.get("nip05").and_then(|v| v.as_str()) {
            if nip05.contains("mostr.pub") || nip05.contains("mostr-pub") {
                debug!("Skipping mostr.pub account");
                return self.get_delete_if_exists(event.pubkey, &subdomain).await;
            }
        }

        // Check for "fields" array (common in ActivityPub)
        if metadata.get("fields").is_some() {
            debug!("Skipping account with 'fields' (likely bridged)");
            return self.get_delete_if_exists(event.pubkey, &subdomain).await;
        }

        // Check for name
        let display_name = metadata.get("display_name").and_then(|v| v.as_str());
        let name = metadata.get("name").and_then(|v| v.as_str());
        if display_name.is_none() && name.is_none() {
            debug!("Rejecting profile - no name");
            return self.get_delete_if_exists(event.pubkey, &subdomain).await;
        }

        // Check for about field
        let about = metadata.get("about").and_then(|v| v.as_str());
        if about.map(|s| s.trim().is_empty()).unwrap_or(true) {
            debug!("Rejecting profile - no about");
            return self.get_delete_if_exists(event.pubkey, &subdomain).await;
        }

        // Verify profile picture
        let Some(image_info) = self.verify_profile_picture(&metadata).await else {
            debug!("Rejecting profile - no picture");
            return self.get_delete_if_exists(event.pubkey, &subdomain).await;
        };

        // Use gossip client to verify user has published events and fetch relay preferences
        if let Some(gossip_client) = gossip_client {
            // Fetch all three kinds (TextNote, relay list, DM relays) via gossip/outbox
            match self.fetch_user_events(event.pubkey, gossip_client).await {
                Ok(events) => {
                    let mut has_text_note = false;
                    let mut relay_events = Vec::new();
                    
                    // Check what we found
                    debug!("Fetched {} total events for {}", events.len(), event.pubkey.to_hex()[..8].to_string() + "...");
                    for fetched_event in events {
                        match fetched_event.kind.as_u16() {
                            1 => { // TextNote
                                has_text_note = true;
                                debug!("Found TextNote for {} via outbox", event.pubkey.to_hex()[..8].to_string() + "...");
                            }
                            10002 => { // Relay list
                                debug!("Found relay list for {}", event.pubkey.to_hex()[..8].to_string() + "...");
                                relay_events.push((fetched_event, "relay list"));
                            }
                            10050 => { // DM relay metadata
                                debug!("Found DM relay metadata for {}", event.pubkey.to_hex()[..8].to_string() + "...");
                                relay_events.push((fetched_event, "DM relays"));
                            }
                            kind => {
                                debug!("Unexpected event kind {} in fetch results", kind);
                            }
                        }
                    }
                    
                    // Only accept profile if they have published a TextNote
                    if !has_text_note {
                        debug!("Rejecting profile {} - no TextNote found via outbox", 
                            event.pubkey.to_hex()[..8].to_string() + "...");
                        return self.get_delete_if_exists(event.pubkey, &subdomain).await;
                    }
                    
                    // Profile passed - prepare store commands
                    let mut commands = vec![StoreCommand::SaveSignedEvent(
                        Box::new(event.clone()),
                        subdomain.clone(),
                    )];
                    
                    // Add relay events since profile was accepted
                    for (relay_event, event_type) in relay_events {
                        debug!("Saving {} for accepted profile {}", 
                            event_type, event.pubkey.to_hex()[..8].to_string() + "...");
                        commands.push(StoreCommand::SaveSignedEvent(
                            Box::new(relay_event), 
                            subdomain.clone()
                        ));
                    }
                    
                    debug!(
                        "✅ Profile {} passed all checks ({}x{} image, has published TextNote)",
                        event.pubkey.to_hex()[..8].to_string() + "...",
                        image_info.width,
                        image_info.height
                    );
                    
                    Ok(commands)
                }
                Err(e) => {
                    debug!("Failed to verify outbox for {}: {}", 
                        event.pubkey.to_hex()[..8].to_string() + "...", e);
                    // Treat verification errors as no activity to err on the side of caution
                    self.get_delete_if_exists(event.pubkey, &subdomain).await
                }
            }
        } else {
            // No gossip client available - reject
            debug!("Rejecting profile {} - no gossip client available for verification", 
                event.pubkey.to_hex()[..8].to_string() + "...");
            self.get_delete_if_exists(event.pubkey, &subdomain).await
        }
    }

    /// Verify profile picture and return ImageInfo if it passes validation
    async fn verify_profile_picture(&self, metadata: &serde_json::Value) -> Option<ImageInfo> {
        let picture = metadata.get("picture").and_then(|v| v.as_str())?;

        if picture.trim().is_empty() {
            debug!("Rejecting profile - empty picture URL");
            return None;
        }

        match self.image_validator.check(picture).await {
            Ok(Some(image_info)) => Some(image_info),
            Ok(None) => {
                debug!("Rejecting profile - image doesn't meet standards");
                None
            }
            Err(e) => {
                warn!("Failed to validate image: {}", e);
                None
            }
        }
    }

    /// Fetch user events (TextNote, relay list, DM relays) using separate queries
    async fn fetch_user_events(
        &self,
        pubkey: PublicKey,
        client: &Client,
    ) -> Result<Vec<Event>, Box<dyn std::error::Error + Send + Sync>> {
        let mut all_events = Vec::new();
        let timeout = std::time::Duration::from_secs(5);
        
        // Query 1: Check for any TextNote (limit 1 since we only need to verify existence)
        let text_note_filter = Filter::new()
            .author(pubkey)
            .kind(Kind::TextNote)
            .limit(1);
            
        if let Ok(events) = client.fetch_events(text_note_filter, timeout).await {
            all_events.extend(events.into_iter());
        }
        
        // Query 2: Fetch replaceable events (relay list and DM relays)
        let relay_filter = Filter::new()
            .author(pubkey)
            .kinds([
                Kind::Custom(10002), // Relay list
                Kind::Custom(10050), // DM relay list
            ])
            .limit(2); // At most one of each replaceable event
            
        match client.fetch_events(relay_filter, timeout).await {
            Ok(events) => {
                debug!("Found {} relay events for {}", events.len(), pubkey.to_hex()[..8].to_string() + "...");
                all_events.extend(events.into_iter());
            }
            Err(e) => {
                debug!("Failed to fetch relay events for {}: {}", pubkey.to_hex()[..8].to_string() + "...", e);
            }
        }

        Ok(all_events)
    }

}

#[async_trait]
impl EventProcessor<()> for ProfileQualityFilter {
    async fn handle_event(
        &self,
        event: Event,
        _custom_state: &mut (),
        context: EventContext<'_>,
    ) -> Result<Vec<StoreCommand>, Error> {
        // No gossip client for inbound WebSocket events
        self.apply_filter(event, context.subdomain.clone())
            .await
            .map_err(|e| Error::internal(format!("Filter error: {}", e)))
    }
}
