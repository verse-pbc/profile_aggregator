//! Global state management

use nostr_relay_builder::RelayDatabase;
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use tracing::{error, info};

// Global state for oldest timestamp
pub static OLDEST_TIMESTAMP: RwLock<Option<u64>> = RwLock::new(None);
// Global state for profile count
pub static PROFILE_COUNT: RwLock<usize> = RwLock::new(0);

/// Find the oldest timestamp in the database using binary search
pub async fn find_oldest_timestamp(
    database: &Arc<RelayDatabase>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    info!("Finding oldest timestamp using binary search...");

    let scope = nostr_lmdb::Scope::Default;
    let now = Timestamp::now().as_u64();
    const YEAR_SECONDS: u64 = 365 * 24 * 60 * 60;

    // Start by going back year by year to find a period with no events
    let mut years_back = 1;
    let mut found_empty_period = false;
    let mut last_year_with_events = 0;

    loop {
        let until = Timestamp::from(now.saturating_sub((years_back - 1) * YEAR_SECONDS));

        // Use only until to check if any events exist before this timestamp
        let filter = Filter::new().kind(Kind::Metadata).until(until).limit(1);

        match database.query(vec![filter], &scope).await {
            Ok(events) => {
                if events.into_iter().next().is_some() {
                    // Found events, go back another year
                    info!("Found events {} years back, going further...", years_back);
                    last_year_with_events = years_back;
                    years_back += 1;
                } else {
                    // No events found this far back
                    info!("No events found {} years back", years_back);
                    found_empty_period = true;
                    break;
                }
            }
            Err(e) => {
                error!("Error querying for oldest timestamp: {}", e);
                return Err(e.into());
            }
        }

        // Safety limit
        if years_back > 10 {
            info!("Reached 10 years back, stopping search");
            break;
        }
    }

    // Now binary search within the period to find the exact oldest timestamp
    let search_end = now.saturating_sub(last_year_with_events.saturating_sub(1) * YEAR_SECONDS);
    let search_start = if found_empty_period {
        now.saturating_sub(years_back * YEAR_SECONDS)
    } else {
        0 // If we went back 10 years and still found events, search from beginning of time
    };

    let mut left = search_start;
    let mut right = search_end;
    let mut oldest_found = right;

    info!("Binary searching between {} and {}", left, right);

    while left < right {
        let mid = left + (right - left) / 2;

        // Check if there are any events before mid timestamp
        let filter = Filter::new()
            .kind(Kind::Metadata)
            .until(Timestamp::from(mid))
            .limit(1);

        match database.query(vec![filter], &scope).await {
            Ok(events) => {
                if let Some(event) = events.into_iter().next() {
                    // Found an event, so there might be older ones
                    oldest_found = event.created_at.as_u64();
                    right = mid;
                } else {
                    // No events before mid, search in the right half
                    left = mid + 1;
                }
            }
            Err(e) => {
                error!("Error during binary search: {}", e);
                return Err(e.into());
            }
        }

        // Precision threshold - stop when we're within a day
        if right - left < 86400 {
            break;
        }
    }

    // Do one final query to get the actual oldest event
    let final_filter = Filter::new()
        .kind(Kind::Metadata)
        .until(Timestamp::from(oldest_found + 86400)) // Add a day buffer
        .limit(1);

    if let Ok(events) = database.query(vec![final_filter], &scope).await {
        if let Some(event) = events.into_iter().next() {
            oldest_found = event.created_at.as_u64();
        }
    }

    let span_days = (now - oldest_found) / 86400;
    info!(
        "Oldest timestamp found: {} ({} days ago)",
        oldest_found, span_days
    );

    Ok(oldest_found)
}

/// Update the cached profile count
pub async fn update_profile_count(
    database: &Arc<RelayDatabase>,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let scope = nostr_lmdb::Scope::Default;
    let filter = Filter::new().kind(Kind::Metadata);
    let count = database.count(vec![filter], &scope).await?;
    Ok(count)
}

/// Initialize global state and start background tasks
pub fn initialize_state(database: Arc<RelayDatabase>) {
    // Initialize oldest timestamp
    let db_clone = database.clone();
    tokio::spawn(async move {
        match find_oldest_timestamp(&db_clone).await {
            Ok(oldest) => {
                *OLDEST_TIMESTAMP.write().unwrap() = Some(oldest);
            }
            Err(e) => {
                error!("Failed to find oldest timestamp: {}", e);
            }
        }
    });

    // Initialize and periodically update profile count
    tokio::spawn(async move {
        loop {
            match update_profile_count(&database).await {
                Ok(count) => {
                    *PROFILE_COUNT.write().unwrap() = count;
                    info!("Updated profile count: {} profiles", count);
                }
                Err(e) => {
                    error!("Failed to update profile count: {}", e);
                }
            }
            // Wait 5 minutes before next update
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
}
