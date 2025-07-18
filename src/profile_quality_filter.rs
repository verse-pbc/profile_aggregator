//! Profile quality filter implementation

use crate::avatar_sync::AvatarSyncClient;
use crate::profile_image_validator::{ImageInfo, ProfileImageValidator};
use crate::rate_limit_manager::RateLimitManager;
use async_trait::async_trait;
use nostr_sdk::prelude::*;
use parking_lot;
use relay_builder::{Error, EventContext, EventProcessor, RelayDatabase, Result, StoreCommand};
use serde::{Deserialize, Serialize};
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;
use tracing::{debug, warn};
use url::Url;

/// Error types for profile validation
#[derive(Debug)]
pub enum ProfileValidationError {
    RateLimited {
        domain: String,
        retry_after: Option<u64>,
    },
    InvalidImage(String),
    InvalidProfile(String),
    NetworkError(String),
}

impl fmt::Display for ProfileValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimited {
                domain,
                retry_after,
            } => {
                if let Some(seconds) = retry_after {
                    write!(f, "Rate limited by {domain}: retry after {seconds}s")
                } else {
                    write!(f, "Rate limited by {domain}")
                }
            }
            Self::InvalidImage(msg) => write!(f, "Invalid image: {msg}"),
            Self::InvalidProfile(msg) => write!(f, "Invalid profile: {msg}"),
            Self::NetworkError(msg) => write!(f, "Network error: {msg}"),
        }
    }
}

impl StdError for ProfileValidationError {}

#[derive(Debug, Deserialize, Serialize)]
pub struct NostrProfileData {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub banner: Option<String>,
    pub nip05: Option<String>,
    pub lud16: Option<String>,
    pub website: Option<String>,
    #[serde(default)]
    pub fields: Option<Vec<serde_json::Value>>,
}

/// Profile quality filter for validating user profiles
#[derive(Clone)]
pub struct ProfileQualityFilter {
    image_validator: Arc<ProfileImageValidator>,
    database: Arc<RelayDatabase>,
    skip_mostr: bool,
    skip_fields: bool,
    gossip_client: Option<Arc<Client>>,
    avatar_sync_client: Option<Arc<AvatarSyncClient>>,
}

impl fmt::Debug for ProfileQualityFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileQualityFilter")
            .field("image_validator", &"ProfileImageValidator")
            .field("skip_mostr", &self.skip_mostr)
            .field("skip_fields", &self.skip_fields)
            .field("gossip_client", &self.gossip_client.is_some())
            .field("avatar_sync_client", &self.avatar_sync_client.is_some())
            .finish()
    }
}

impl ProfileQualityFilter {
    pub fn new(database: Arc<RelayDatabase>) -> Self {
        Self::with_options(database, true, true)
    }

    pub fn with_options(database: Arc<RelayDatabase>, skip_mostr: bool, skip_fields: bool) -> Self {
        let rate_limit_manager = Arc::new(RateLimitManager::new());
        let avatar_sync_client = AvatarSyncClient::new_default().ok().map(Arc::new);

        Self {
            // Single image validator with shared rate limiter
            image_validator: Arc::new(
                ProfileImageValidator::with_rate_limiter(
                    300,  // min_width
                    600,  // min_height
                    true, // allow_animated
                    rate_limit_manager,
                )
                .expect("Failed to create ProfileImageValidator"),
            ),
            database,
            skip_mostr,
            skip_fields,
            gossip_client: None,
            avatar_sync_client,
        }
    }

    /// Create a new profile quality filter with a gossip client
    pub fn with_gossip_client(
        database: Arc<RelayDatabase>,
        skip_mostr: bool,
        skip_fields: bool,
        gossip_client: Arc<Client>,
    ) -> Self {
        let rate_limit_manager = Arc::new(RateLimitManager::new());
        let avatar_sync_client = AvatarSyncClient::new_default().ok().map(Arc::new);

        Self {
            // Single image validator with shared rate limiter
            image_validator: Arc::new(
                ProfileImageValidator::with_rate_limiter(
                    300,  // min_width
                    600,  // min_height
                    true, // allow_animated
                    rate_limit_manager,
                )
                .expect("Failed to create ProfileImageValidator"),
            ),
            database,
            skip_mostr,
            skip_fields,
            gossip_client: Some(gossip_client),
            avatar_sync_client,
        }
    }

    /// Check if a metadata event exists for the given author
    async fn check_existing_metadata(
        &self,
        author: PublicKey,
        subdomain: &nostr_lmdb::Scope,
    ) -> std::result::Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let filter = Filter::new().author(author).kind(Kind::Metadata);

        let events = self
            .database
            .query(vec![filter], subdomain)
            .await
            .map_err(|e| format!("Failed to query database: {e}"))?;

        Ok(!events.is_empty())
    }

    /// Get delete command if existing pubkey should be removed
    async fn get_delete_if_exists(
        &self,
        author: PublicKey,
        subdomain: &nostr_lmdb::Scope,
    ) -> std::result::Result<Vec<StoreCommand>, Box<dyn std::error::Error + Send + Sync>> {
        if self.check_existing_metadata(author, subdomain).await? {
            let cleanup_filter = Filter::new().author(author);
            Ok(vec![StoreCommand::DeleteEvents(
                cleanup_filter,
                subdomain.clone(),
                None,
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
    ) -> std::result::Result<Vec<StoreCommand>, Box<dyn std::error::Error + Send + Sync>> {
        self.apply_filter_internal(event, subdomain, Some(gossip_client))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Apply the quality filter to an event (without outbox verification)
    pub async fn apply_filter(
        &self,
        event: Event,
        subdomain: nostr_lmdb::Scope,
    ) -> std::result::Result<Vec<StoreCommand>, Box<dyn std::error::Error + Send + Sync>> {
        self.apply_filter_internal(event, subdomain, None)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Internal filter application logic
    async fn apply_filter_internal(
        &self,
        event: Event,
        subdomain: nostr_lmdb::Scope,
        gossip_client: Option<&Client>,
    ) -> std::result::Result<Vec<StoreCommand>, ProfileValidationError> {
        // Handle relay list events (kind 10002) - only accept if the author has metadata
        if event.kind == Kind::Custom(10002) {
            // Check if this pubkey has existing metadata in the database
            if self
                .check_existing_metadata(event.pubkey, &subdomain)
                .await
                .map_err(|e| ProfileValidationError::NetworkError(e.to_string()))?
            {
                return Ok(vec![StoreCommand::SaveSignedEvent(
                    Box::new(event),
                    subdomain,
                    None,
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
            return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                Ok(cmds) => Ok(cmds),
                Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
            };
        }

        // Parse metadata
        let profile_data: NostrProfileData = match serde_json::from_str(&event.content) {
            Ok(m) => m,
            Err(e) => {
                debug!("Invalid metadata JSON: {}", e);
                return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                    Ok(cmds) => Ok(cmds),
                    Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
                };
            }
        };

        // Check for mostr.pub NIP-05
        if self.skip_mostr {
            if let Some(nip05) = &profile_data.nip05 {
                if nip05.contains("mostr.pub") || nip05.contains("mostr-pub") {
                    debug!("Skipping mostr.pub account");
                    return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                        Ok(cmds) => Ok(cmds),
                        Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
                    };
                }
            }
        }

        // Check for "fields" array (common in ActivityPub)
        if self.skip_fields && profile_data.fields.is_some() {
            debug!("Skipping account with 'fields' (likely bridged)");
            return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                Ok(cmds) => Ok(cmds),
                Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
            };
        }

        // Check for name
        if profile_data.display_name.is_none() && profile_data.name.is_none() {
            debug!("Rejecting profile - no name");
            return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                Ok(cmds) => Ok(cmds),
                Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
            };
        }

        // Check for about field
        if profile_data
            .about
            .as_ref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            debug!("Rejecting profile - no about");
            return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                Ok(cmds) => Ok(cmds),
                Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
            };
        }

        // Pre-check rate limits for picture
        if let Some(picture) = &profile_data.picture {
            if let Ok(true) = self.image_validator.is_domain_rate_limited(picture).await {
                let domain = self.extract_domain(picture);
                return Err(ProfileValidationError::RateLimited {
                    domain,
                    retry_after: None,
                });
            }
        }

        // Verify profile picture
        let Some(image_info) = self.verify_profile_picture(&profile_data).await else {
            debug!("Rejecting profile - no picture");
            return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                Ok(cmds) => Ok(cmds),
                Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
            };
        };

        // Use gossip client to verify user has published events and fetch relay preferences
        if let Some(gossip_client) = gossip_client {
            // Fetch all three kinds (TextNote, relay list, DM relays) via gossip/outbox
            match self.fetch_user_events(event.pubkey, gossip_client).await {
                Ok((events, relays_with_content)) => {
                    let mut has_text_note = false;
                    let mut relay_events = Vec::new();
                    let mut has_relay_list = false;

                    // Check what we found
                    debug!(
                        "Fetched {} total events for {} from {} relays",
                        events.len(),
                        event.pubkey.to_hex()[..8].to_string() + "...",
                        relays_with_content.len()
                    );
                    for fetched_event in events {
                        match fetched_event.kind.as_u16() {
                            1 => {
                                // TextNote
                                has_text_note = true;
                                debug!(
                                    "Found TextNote for {} via outbox",
                                    event.pubkey.to_hex()[..8].to_string() + "..."
                                );
                            }
                            10002 => {
                                // Relay list
                                debug!(
                                    "Found relay list for {}",
                                    event.pubkey.to_hex()[..8].to_string() + "..."
                                );
                                relay_events.push((fetched_event, "relay list"));
                                has_relay_list = true;
                            }
                            10050 => {
                                // DM relay metadata
                                debug!(
                                    "Found DM relay metadata for {}",
                                    event.pubkey.to_hex()[..8].to_string() + "..."
                                );
                                relay_events.push((fetched_event, "DM relays"));
                            }
                            kind => {
                                debug!("Unexpected event kind {} in fetch results", kind);
                            }
                        }
                    }

                    // Only accept profile if they have published a TextNote
                    if !has_text_note {
                        debug!(
                            "Rejecting profile {} - no TextNote found via outbox",
                            event.pubkey.to_hex()[..8].to_string() + "..."
                        );
                        return match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                            Ok(cmds) => Ok(cmds),
                            Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
                        };
                    }

                    // Profile passed - prepare store commands
                    let mut commands = vec![StoreCommand::SaveSignedEvent(
                        Box::new(event.clone()),
                        subdomain.clone(),
                        None,
                    )];

                    // Trigger avatar sync for the accepted profile
                    if let Some(ref avatar_sync_client) = self.avatar_sync_client {
                        avatar_sync_client.trigger_sync(event.pubkey);
                    }

                    // Add relay events since profile was accepted
                    for (relay_event, event_type) in relay_events {
                        debug!(
                            "Saving {} for accepted profile {}",
                            event_type,
                            event.pubkey.to_hex()[..8].to_string() + "..."
                        );
                        commands.push(StoreCommand::SaveSignedEvent(
                            Box::new(relay_event),
                            subdomain.clone(),
                            None,
                        ));
                    }

                    // If no relay list was found but we found content, log it
                    if !has_relay_list && has_text_note {
                        debug!(
                            "User {} has verified content but no published relay lists",
                            event.pubkey.to_hex()[..8].to_string() + "..."
                        );
                    }

                    debug!(
                        "Profile {} verified: {}x{} image, has text note",
                        event.pubkey.to_hex()[..8].to_string() + "...",
                        image_info.width,
                        image_info.height
                    );

                    Ok(commands)
                }
                Err(e) => {
                    debug!(
                        "Failed to verify outbox for {}: {}",
                        event.pubkey.to_hex()[..8].to_string() + "...",
                        e
                    );
                    // Treat verification errors as no activity to err on the side of caution
                    match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                        Ok(cmds) => Ok(cmds),
                        Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
                    }
                }
            }
        } else {
            // No gossip client available - reject
            debug!(
                "Rejecting profile {} - no gossip client available for verification",
                event.pubkey.to_hex()[..8].to_string() + "..."
            );
            match self.get_delete_if_exists(event.pubkey, &subdomain).await {
                Ok(cmds) => Ok(cmds),
                Err(e) => Err(ProfileValidationError::NetworkError(e.to_string())),
            }
        }
    }

    /// Verify profile picture and return ImageInfo if it passes validation
    async fn verify_profile_picture(&self, profile_data: &NostrProfileData) -> Option<ImageInfo> {
        let picture = profile_data.picture.as_ref()?;

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

    fn extract_domain(&self, url: &str) -> String {
        url.parse::<Url>()
            .ok()
            .and_then(|u| u.domain().map(String::from))
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Fetch user events (TextNote, relay list, DM relays) using separate queries
    /// Returns (events, relays_with_content)
    async fn fetch_user_events(
        &self,
        pubkey: PublicKey,
        client: &Client,
    ) -> std::result::Result<(Vec<Event>, Vec<String>), Box<dyn std::error::Error + Send + Sync>>
    {
        let mut all_events = Vec::new();
        let timeout = std::time::Duration::from_secs(5);

        // Query 1: Check for any TextNote (limit 1 since we only need to verify existence)
        let text_note_filter = Filter::new().author(pubkey).kind(Kind::TextNote).limit(1);

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
                debug!(
                    "Found {} relay events for {}",
                    events.len(),
                    pubkey.to_hex()[..8].to_string() + "..."
                );
                all_events.extend(events.into_iter());
            }
            Err(e) => {
                debug!(
                    "Failed to fetch relay events for {}: {}",
                    pubkey.to_hex()[..8].to_string() + "...",
                    e
                );
            }
        }

        // Track which relays actually had content for this user
        let relays_with_content = Vec::new();

        // For now, we're not tracking specific relay sources
        // This would require more complex subscription handling

        Ok((all_events, relays_with_content))
    }
}

#[async_trait]
impl EventProcessor<()> for ProfileQualityFilter {
    async fn handle_event(
        &self,
        event: Event,
        _custom_state: Arc<parking_lot::RwLock<()>>,
        context: EventContext<'_>,
    ) -> Result<Vec<StoreCommand>> {
        // Use the stored gossip client if available
        // context.subdomain is already a &Scope, so we just need to clone it
        let result = if let Some(ref gossip_client) = self.gossip_client {
            self.apply_filter_with_gossip(event.clone(), context.subdomain.clone(), gossip_client)
                .await
        } else {
            self.apply_filter(event.clone(), context.subdomain.clone())
                .await
        };

        match result {
            Ok(commands) => {
                // If no commands are returned, the event was rejected
                if commands.is_empty() {
                    Err(Error::event_error(
                        "blocked: profile does not meet quality standards",
                        event.id,
                    ))
                } else {
                    println!("commands: {commands:?}");
                    Ok(commands)
                }
            }
            Err(e) => Err(Error::internal(format!("Filter error: {e}"))),
        }
    }
}
