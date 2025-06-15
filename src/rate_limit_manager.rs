use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use reqwest::Url;
use std::collections::HashMap;
use std::error::Error;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, warn};

type DomainRateLimiter = RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;

/// Manages rate limiting for domains across the application
pub struct RateLimitManager {
    // Rate limiters per domain
    domain_rate_limiters: Arc<Mutex<HashMap<String, Arc<DomainRateLimiter>>>>,
}

impl RateLimitManager {
    pub fn new() -> Self {
        Self {
            domain_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check if a domain is currently rate limited without making a request
    pub async fn is_domain_rate_limited(
        &self,
        url: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        // Extract domain from URL
        let parsed_url = Url::parse(url)?;
        let domain = parsed_url
            .domain()
            .ok_or("Invalid URL: no domain")?
            .to_string();

        // Check if we have a rate limiter for this domain
        let limiters = self.domain_rate_limiters.lock().await;
        if let Some(rate_limiter) = limiters.get(&domain) {
            // Check if we can make a request now
            match rate_limiter.check_key(&domain) {
                Ok(_) => Ok(false), // Not rate limited
                Err(not_until) => {
                    let wait_time = not_until.wait_time_from(Clock::now(&DefaultClock::default()));
                    debug!("Domain {} is rate limited for {:?}", domain, wait_time);
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
        let domain = parsed_url
            .domain()
            .ok_or("Invalid URL: no domain")?
            .to_string();

        let limiters = self.domain_rate_limiters.lock().await;
        if let Some(rate_limiter) = limiters.get(&domain) {
            match rate_limiter.check_key(&domain) {
                Ok(_) => Ok(None), // Not rate limited
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
        let mut limiters = self.domain_rate_limiters.lock().await;

        // Calculate appropriate quota based on retry-after
        let quota = if let Some(seconds) = retry_after_seconds {
            // If we need to wait N seconds, allow 1 request per N seconds
            let requests_per_minute = (60.0 / seconds.max(1) as f64).ceil() as u32;
            Quota::per_minute(NonZeroU32::new(requests_per_minute.max(1)).unwrap())
        } else {
            // Default conservative rate limit
            Quota::per_minute(NonZeroU32::new(10).unwrap())
        };

        limiters.insert(domain.to_string(), Arc::new(RateLimiter::keyed(quota)));

        warn!(
            "Updated rate limiter for {}: {:?}",
            domain,
            retry_after_seconds
                .map(|s| format!("{}s", s))
                .unwrap_or("default".to_string())
        );
    }

    /// Extract domain from URL
    pub fn extract_domain(&self, url: &str) -> Option<String> {
        Url::parse(url)
            .ok()
            .and_then(|u| u.domain().map(String::from))
    }

    /// Get or create a rate limiter for a domain
    pub async fn get_or_create_limiter(&self, domain: &str) -> Arc<DomainRateLimiter> {
        let mut limiters = self.domain_rate_limiters.lock().await;
        limiters
            .entry(domain.to_string())
            .or_insert_with(|| {
                // Default: 100 requests per minute per domain
                Arc::new(RateLimiter::keyed(Quota::per_minute(
                    NonZeroU32::new(100).unwrap(),
                )))
            })
            .clone()
    }

    /// Check rate limit for a domain
    pub async fn check_rate_limit(&self, domain: &str) -> Result<(), Duration> {
        let limiter = self.get_or_create_limiter(domain).await;
        let domain_string = domain.to_string();

        match limiter.check_key(&domain_string) {
            Ok(_) => Ok(()),
            Err(not_until) => {
                let wait_time = not_until.wait_time_from(Clock::now(&DefaultClock::default()));
                debug!(
                    "Rate limited for domain {}: waiting {:?}",
                    domain, wait_time
                );
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
