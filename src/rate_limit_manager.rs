use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use reqwest::Url;
use std::collections::HashMap;
use std::error::Error;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

type DomainRateLimiter = RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;

/// Tracks rate limit state for a domain
struct DomainRateLimitState {
    limiter: Arc<DomainRateLimiter>,
    consecutive_429s: u32,
    last_429_time: Option<Instant>,
}

/// Manages rate limiting for domains across the application
pub struct RateLimitManager {
    // Rate limiters and state per domain
    domain_states: Arc<Mutex<HashMap<String, DomainRateLimitState>>>,
}

impl RateLimitManager {
    pub fn new() -> Self {
        Self {
            domain_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get the default rate limit for a domain
    fn get_default_quota_for_domain(domain: &str) -> Quota {
        match domain {
            // imgur: 12,500 requests/day = ~8.68/minute, be conservative with 8/minute
            "i.imgur.com" => Quota::per_minute(NonZeroU32::new(8).unwrap()),
            // Most other services: default to 30/minute
            _ => Quota::per_minute(NonZeroU32::new(30).unwrap()),
        }
    }

    /// Normalize domain for rate limiting (handle subdomains that share limits)
    fn normalize_domain(domain: &str) -> &str {
        // For now, treat each domain separately since imgur.com gallery links
        // are different from i.imgur.com direct image links
        domain
    }

    /// Check if a domain is currently rate limited without making a request
    pub async fn is_domain_rate_limited(
        &self,
        url: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        // Extract domain from URL
        let parsed_url = Url::parse(url)?;
        let domain = parsed_url.domain().ok_or("Invalid URL: no domain")?;
        let normalized_domain = Self::normalize_domain(domain).to_string();

        // Check if we have a rate limiter for this domain
        let states = self.domain_states.lock().await;
        if let Some(state) = states.get(&normalized_domain) {
            // Check if we can make a request now
            match state.limiter.check_key(&normalized_domain) {
                Ok(_) => Ok(false), // Not rate limited
                Err(not_until) => {
                    let wait_time = not_until.wait_time_from(Clock::now(&DefaultClock::default()));
                    debug!(
                        "Domain {} is rate limited for {:?}",
                        normalized_domain, wait_time
                    );
                    Ok(true) // Rate limited
                }
            }
        } else {
            // No rate limiter means we haven't hit limits yet
            Ok(false)
        }
    }

    /// Get the wait time until a domain is no longer rate limited
    pub async fn get_rate_limit_wait_time(
        &self,
        url: &str,
    ) -> Result<Option<Duration>, Box<dyn Error + Send + Sync>> {
        let parsed_url = Url::parse(url)?;
        let domain = parsed_url.domain().ok_or("Invalid URL: no domain")?;
        let normalized_domain = Self::normalize_domain(domain).to_string();

        let states = self.domain_states.lock().await;
        if let Some(state) = states.get(&normalized_domain) {
            match state.limiter.check_key(&normalized_domain) {
                Ok(_) => Ok(None),
                Err(not_until) => {
                    let wait_time = not_until.wait_time_from(Clock::now(&DefaultClock::default()));
                    Ok(Some(wait_time))
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Update rate limiter based on 429 response
    pub async fn update_rate_limiter(&self, domain: &str, retry_after_seconds: Option<u64>) {
        let normalized_domain = Self::normalize_domain(domain);
        let mut states = self.domain_states.lock().await;
        let now = Instant::now();

        // Get or create state for this domain
        let state = states
            .entry(normalized_domain.to_string())
            .or_insert_with(|| DomainRateLimitState {
                limiter: Arc::new(RateLimiter::keyed(Self::get_default_quota_for_domain(
                    normalized_domain,
                ))),
                consecutive_429s: 0,
                last_429_time: None,
            });

        // Reset consecutive 429s if it's been more than 5 minutes since last 429
        if let Some(last_time) = state.last_429_time {
            if now.duration_since(last_time) > Duration::from_secs(300) {
                state.consecutive_429s = 0;
            }
        }

        // Update consecutive 429 count
        state.consecutive_429s += 1;
        state.last_429_time = Some(now);

        // Calculate appropriate quota based on retry-after or exponential backoff
        let (quota, wait_description) = if let Some(seconds) = retry_after_seconds {
            // Use the server-provided retry-after
            let requests_per_minute = (60.0 / seconds.max(1) as f64).ceil() as u32;
            (
                Quota::per_minute(NonZeroU32::new(requests_per_minute.max(1)).unwrap()),
                format!("{}s (from header)", seconds),
            )
        } else {
            // Exponential backoff starting from 30s: 30s, 60s, 120s, 240s...
            // For imgur: 12,500/day = ~8.68/minute, so we start conservative
            let base_seconds = 30u64;
            let backoff_seconds =
                (base_seconds * (1u64 << (state.consecutive_429s - 1).min(4))).min(600); // Cap at 10 minutes
            let requests_per_minute = (60.0 / backoff_seconds as f64).ceil() as u32;
            (
                Quota::per_minute(NonZeroU32::new(requests_per_minute.max(1)).unwrap()),
                format!(
                    "{}s (exponential backoff, attempt #{})",
                    backoff_seconds, state.consecutive_429s
                ),
            )
        };

        state.limiter = Arc::new(RateLimiter::keyed(quota));

        if domain != normalized_domain {
            warn!(
                "Updated rate limiter for {} (normalized: {}): {}",
                domain, normalized_domain, wait_description
            );
        } else {
            warn!("Updated rate limiter for {}: {}", domain, wait_description);
        }
    }

    /// Extract domain from URL
    pub fn extract_domain(&self, url: &str) -> Option<String> {
        Url::parse(url)
            .ok()
            .and_then(|u| u.domain().map(String::from))
    }

    /// Get or create a rate limiter for a domain
    async fn get_or_create_limiter(&self, domain: &str) -> Arc<DomainRateLimiter> {
        let normalized_domain = Self::normalize_domain(domain);
        let now = Instant::now();
        let mut states = self.domain_states.lock().await;
        let state = states
            .entry(normalized_domain.to_string())
            .or_insert_with(|| DomainRateLimitState {
                limiter: Arc::new(RateLimiter::keyed(Self::get_default_quota_for_domain(
                    normalized_domain,
                ))),
                consecutive_429s: 0,
                last_429_time: None,
            });

        // Reset consecutive 429s if it's been more than 5 minutes since last 429
        if let Some(last_time) = state.last_429_time {
            if now.duration_since(last_time) > Duration::from_secs(300) {
                state.consecutive_429s = 0;
                state.last_429_time = None;
            }
        }

        state.limiter.clone()
    }

    /// Check rate limit for a domain
    pub async fn check_rate_limit(&self, domain: &str) -> Result<(), Duration> {
        let normalized_domain = Self::normalize_domain(domain);
        let limiter = self.get_or_create_limiter(normalized_domain).await;
        let normalized_string = normalized_domain.to_string();

        match limiter.check_key(&normalized_string) {
            Ok(_) => Ok(()),
            Err(not_until) => {
                let wait_time = not_until.wait_time_from(Clock::now(&DefaultClock::default()));
                if domain != normalized_domain {
                    debug!(
                        "Rate limited for domain {} (normalized: {}): waiting {:?}",
                        domain, normalized_domain, wait_time
                    );
                } else {
                    debug!(
                        "Rate limited for domain {}: waiting {:?}",
                        domain, wait_time
                    );
                }
                Err(wait_time)
            }
        }
    }
}

impl Default for RateLimitManager {
    fn default() -> Self {
        Self::new()
    }
}
