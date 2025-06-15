use crate::rate_limit_manager::RateLimitManager;
use image::{ImageDecoder, ImageFormat};
use reqwest::{Client, Response, Url};
use std::error::Error;
use std::io::Cursor;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use svgtypes::ViewBox;
use tracing::{debug, warn};

#[derive(Debug)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub is_animated: bool,
}

pub struct ProfileImageValidator {
    client: Client,
    min_width: u32,
    min_height: u32,
    allow_animated: bool,
    rate_limit_manager: Arc<RateLimitManager>,
}

impl ProfileImageValidator {
    pub fn new(min_width: u32, min_height: u32, allow_animated: bool) -> Self {
        Self::with_rate_limiter(
            min_width,
            min_height,
            allow_animated,
            Arc::new(RateLimitManager::new()),
        )
    }

    pub fn with_rate_limiter(
        min_width: u32,
        min_height: u32,
        allow_animated: bool,
        rate_limit_manager: Arc<RateLimitManager>,
    ) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent("ProfileAggregator/1.0")
                .build()
                .unwrap(),
            min_width,
            min_height,
            allow_animated,
            rate_limit_manager,
        }
    }

    /// Check if a URL's domain is currently rate limited
    pub async fn is_domain_rate_limited(
        &self,
        url: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        self.rate_limit_manager.is_domain_rate_limited(url).await
    }

    /// Get the wait time until a domain is no longer rate limited
    pub async fn get_rate_limit_wait_time(
        &self,
        url: &str,
    ) -> Result<Option<Duration>, Box<dyn Error + Send + Sync>> {
        self.rate_limit_manager.get_rate_limit_wait_time(url).await
    }

    pub async fn check(
        &self,
        url: &str,
    ) -> Result<Option<ImageInfo>, Box<dyn Error + Send + Sync>> {
        // Skip obvious bad URLs
        if url.contains("favicon.ico")
            || url.contains("rss-to-nostr")
            || url.contains("default-avatar")
            || url.contains("placeholder")
        {
            debug!("Rejecting URL with known bad pattern: {}", url);
            return Ok(None);
        }

        // Skip imgur.com gallery links (only accept direct image links from i.imgur.com)
        if url.contains("imgur.com") && !url.contains("i.imgur.com") {
            debug!("Rejecting imgur gallery link (not a direct image): {}", url);
            return Ok(None);
        }

        match self.get_info(url).await {
            Ok(info) => {
                debug!(
                    "Image info for {}: {}x{} {:?} animated={}",
                    url, info.width, info.height, info.format, info.is_animated
                );

                // Check dimensions
                if info.width < self.min_width || info.height < self.min_height {
                    debug!(
                        "Image too small: {}x{} < {}x{}",
                        info.width, info.height, self.min_width, self.min_height
                    );
                    return Ok(None);
                }

                // Check animation if needed
                if !self.allow_animated && info.is_animated {
                    debug!("Animated image rejected");
                    return Ok(None);
                }

                Ok(Some(info))
            }
            Err(e) => {
                // Only warn for non-rate-limit errors
                let error_str = e.to_string();
                if !error_str.contains("Rate limited") && !error_str.contains("429") {
                    debug!("Failed to validate image {}: {}", url, e);
                }
                Ok(None) // Reject images we can't validate
            }
        }
    }

    pub async fn get_info(&self, url: &str) -> Result<ImageInfo, Box<dyn Error + Send + Sync>> {
        // Handle data URLs
        if url.starts_with("data:image/") {
            return self.analyze_data_url(url);
        }

        // Extract domain from URL
        let parsed_url = Url::parse(url)?;
        let domain = parsed_url
            .domain()
            .ok_or("Invalid URL: no domain")?
            .to_string();

        // Check rate limit
        if let Err(wait_time) = self.rate_limit_manager.check_rate_limit(&domain).await {
            debug!(
                "Rate limited for domain {}: waiting {:?}",
                domain, wait_time
            );
            return Err(format!("Rate limited for domain {}", domain).into());
        }

        let response = self.client.get(url).send().await?;

        // Check status code
        if !response.status().is_success() {
            // Special handling for rate limiting
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                // Parse rate limit headers if available
                self.update_rate_limiter_from_headers(&domain, &response)
                    .await;

                warn!("Rate limited by {}: 429 Too Many Requests", domain);
                return Err(format!("Rate limited by {}", domain).into());
            }
            return Err(format!("HTTP error: {}", response.status()).into());
        }

        // Check content type
        let content_type = if let Some(ct) = response.headers().get("content-type") {
            ct.to_str()?.to_lowercase()
        } else {
            String::new()
        };

        // Check if it's an SVG
        if content_type.contains("svg") || url.ends_with(".svg") {
            // Download SVG content to parse dimensions
            let svg_bytes = response.bytes().await?;

            // Parse SVG dimensions
            let svg_content = String::from_utf8_lossy(&svg_bytes);
            let (width, height) = self.parse_svg_dimensions(&svg_content)?;

            return Ok(ImageInfo {
                width,
                height,
                format: ImageFormat::Png, // Use PNG as a placeholder format for SVG
                is_animated: false,
            });
        }

        // Ensure it's an image
        if !content_type.is_empty() && !content_type.starts_with("image/") {
            return Err(format!("Not an image: {}", content_type).into());
        }

        // Limit download size to 10MB
        let max_size = 10 * 1024 * 1024;
        let content_length = response.content_length().unwrap_or(0);
        if content_length > max_size as u64 {
            return Err(format!("Image too large: {} bytes", content_length).into());
        }

        let bytes = response.bytes().await?;

        if bytes.len() > max_size {
            return Err(format!("Image too large: {} bytes", bytes.len()).into());
        }

        let format = image::guess_format(&bytes)?;

        let (width, height, is_animated) = match format {
            ImageFormat::Gif => self.analyze_gif(&bytes)?,
            ImageFormat::WebP => self.analyze_webp(&bytes)?,
            ImageFormat::Png => self.analyze_png(&bytes)?,
            _ => {
                let (w, h) = self.get_dimensions(&bytes)?;
                (w, h, false)
            }
        };

        Ok(ImageInfo {
            width,
            height,
            format,
            is_animated,
        })
    }

    async fn update_rate_limiter_from_headers(&self, domain: &str, response: &Response) {
        let headers = response.headers();

        // Try to get rate limit from headers
        if let Some(retry_after) = headers.get("retry-after") {
            if let Ok(retry_str) = retry_after.to_str() {
                // Retry-After can be seconds or HTTP date
                if let Ok(seconds) = retry_str.parse::<u64>() {
                    // Only update if seconds > 0
                    if seconds > 0 {
                        self.rate_limit_manager
                            .update_rate_limiter(domain, Some(seconds))
                            .await;
                        return;
                    }
                }
            }
        }

        // No valid retry-after header, use default rate limit
        self.rate_limit_manager
            .update_rate_limiter(domain, None)
            .await;
    }

    fn analyze_data_url(&self, url: &str) -> Result<ImageInfo, Box<dyn Error + Send + Sync>> {
        // Parse data URL: data:image/png;base64,iVBORw0KGgo...
        let parts: Vec<&str> = url.splitn(2, ',').collect();
        if parts.len() != 2 {
            return Err("Invalid data URL format".into());
        }

        let data = if parts[0].contains("base64") {
            use base64::{engine::general_purpose, Engine as _};
            general_purpose::STANDARD.decode(parts[1])?
        } else {
            parts[1].as_bytes().to_vec()
        };

        let format = image::guess_format(&data)?;
        let (width, height) = self.get_dimensions(&data)?;

        Ok(ImageInfo {
            width,
            height,
            format,
            is_animated: false, // Assume data URLs are not animated
        })
    }

    fn get_dimensions(&self, data: &[u8]) -> Result<(u32, u32), Box<dyn Error + Send + Sync>> {
        match std::panic::catch_unwind(|| imageinfo::ImageInfo::from_raw_data(data)) {
            Ok(Ok(info)) => {
                return Ok((info.size.width as u32, info.size.height as u32));
            }
            Ok(Err(_)) => {}
            Err(_) => {
                warn!("imageinfo panicked while parsing image data");
            }
        }

        let cursor = Cursor::new(data);
        let reader = image::ImageReader::new(cursor).with_guessed_format()?;
        Ok(reader.into_dimensions()?)
    }

    fn analyze_gif(&self, data: &[u8]) -> Result<(u32, u32, bool), Box<dyn Error + Send + Sync>> {
        use image::codecs::gif::GifDecoder;

        let cursor = Cursor::new(data);
        let decoder = GifDecoder::new(cursor)?;
        let (width, height) = decoder.dimensions();

        // Check for multiple frames by looking for Image Separator bytes
        let mut frame_count = 0;
        for i in 0..data.len().saturating_sub(1) {
            if data[i] == 0x00 && data[i + 1] == 0x2C {
                frame_count += 1;
                if frame_count > 1 {
                    return Ok((width, height, true));
                }
            }
        }

        Ok((width, height, false))
    }

    fn analyze_webp(&self, data: &[u8]) -> Result<(u32, u32, bool), Box<dyn Error + Send + Sync>> {
        let (width, height) = self.get_dimensions(data)?;

        // Check VP8X chunk for animation flag
        let is_animated = data.len() > 30 && &data[12..16] == b"VP8X" && (data[20] & 0x02) != 0;

        Ok((width, height, is_animated))
    }

    fn analyze_png(&self, data: &[u8]) -> Result<(u32, u32, bool), Box<dyn Error + Send + Sync>> {
        use image::codecs::png::PngDecoder;

        let cursor = Cursor::new(data);
        let decoder = PngDecoder::new(cursor)?;
        let (width, height) = decoder.dimensions();

        // Look for acTL chunk (APNG)
        let is_animated = self.has_actl_chunk(data);

        Ok((width, height, is_animated))
    }

    fn has_actl_chunk(&self, data: &[u8]) -> bool {
        if data.len() < 8 {
            return false;
        }

        let mut pos = 8; // Skip PNG signature
        while pos + 8 < data.len() {
            let chunk_len =
                u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize;
            let chunk_type = &data[pos + 4..pos + 8];

            if chunk_type == b"acTL" {
                return true;
            }

            pos += 12 + chunk_len;

            if chunk_type == b"IDAT" || chunk_type == b"IEND" {
                break;
            }
        }
        false
    }

    fn parse_svg_dimensions(
        &self,
        svg_content: &str,
    ) -> Result<(u32, u32), Box<dyn Error + Send + Sync>> {
        // First try to find viewBox attribute
        if let Some(viewbox_start) = svg_content.find("viewBox=") {
            let start = viewbox_start + 9; // Skip 'viewBox="'
            if let Some(end) = svg_content[start..].find('"') {
                let viewbox_str = &svg_content[start..start + end];

                // Parse viewBox using svgtypes
                if let Ok(viewbox) = ViewBox::from_str(viewbox_str) {
                    return Ok((viewbox.w as u32, viewbox.h as u32));
                }
            }
        }

        // Try to find width and height attributes
        let mut width = None;
        let mut height = None;

        // Extract width
        if let Some(width_start) = svg_content.find("width=") {
            let start = width_start + 7; // Skip 'width="'
            if let Some(end) = svg_content[start..].find('"') {
                let width_str = &svg_content[start..start + end];
                // Remove 'px' suffix if present
                let width_str = width_str.trim_end_matches("px");
                if let Ok(w) = width_str.parse::<f64>() {
                    width = Some(w as u32);
                }
            }
        }

        // Extract height
        if let Some(height_start) = svg_content.find("height=") {
            let start = height_start + 8; // Skip 'height="'
            if let Some(end) = svg_content[start..].find('"') {
                let height_str = &svg_content[start..start + end];
                // Remove 'px' suffix if present
                let height_str = height_str.trim_end_matches("px");
                if let Ok(h) = height_str.parse::<f64>() {
                    height = Some(h as u32);
                }
            }
        }

        match (width, height) {
            (Some(w), Some(h)) => Ok((w, h)),
            _ => {
                // If we can't find dimensions, return a reasonable default
                // but log a warning
                warn!("Could not parse SVG dimensions, using default 1000x1000");
                Ok((1000, 1000))
            }
        }
    }
}
