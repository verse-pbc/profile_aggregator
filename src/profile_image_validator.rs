use crate::rate_limit_manager::RateLimitManager;
use image::{ImageDecoder, ImageFormat};
use reqwest::{header::RANGE, Client, Response, StatusCode, Url};
use std::error::Error;
use std::fmt;
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

#[derive(Debug)]
pub enum ValidationError {
    NetworkError(String),
    RateLimited {
        domain: String,
        wait_time: Option<Duration>,
    },
    InvalidImage(String),
    TooLarge(usize),
    TooSmall {
        width: u32,
        height: u32,
    },
    AnimationNotAllowed,
    ParseError(String),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::NetworkError(msg) => write!(f, "Network error: {msg}"),
            ValidationError::RateLimited { domain, wait_time } => match wait_time {
                Some(duration) => {
                    write!(f, "Rate limited by {domain}: retry after {duration:?}")
                }
                None => write!(f, "Rate limited by {domain}"),
            },
            ValidationError::InvalidImage(msg) => write!(f, "Invalid image: {msg}"),
            ValidationError::TooLarge(size) => write!(f, "Image too large: {size} bytes"),
            ValidationError::TooSmall { width, height } => {
                write!(f, "Image too small: {width}x{height}")
            }
            ValidationError::AnimationNotAllowed => write!(f, "Animated images not allowed"),
            ValidationError::ParseError(msg) => write!(f, "Parse error: {msg}"),
        }
    }
}

impl Error for ValidationError {}

pub struct ProfileImageValidator {
    client: Client,
    min_width: u32,
    min_height: u32,
    allow_animated: bool,
    rate_limit_manager: Arc<RateLimitManager>,
    max_header_bytes: usize,
}

impl ProfileImageValidator {
    pub fn new(
        min_width: u32,
        min_height: u32,
        allow_animated: bool,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
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
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("ProfileAggregator/1.0")
            .build()?;

        Ok(Self {
            client,
            min_width,
            min_height,
            allow_animated,
            rate_limit_manager,
            max_header_bytes: 8192, // 8KB should be enough for most image headers
        })
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
        // Parse URL first to validate it
        let parsed_url = match Url::parse(url) {
            Ok(u) => u,
            Err(e) => {
                debug!("Invalid URL {}: {}", url, e);
                return Ok(None);
            }
        };

        // Extract domain properly
        let domain = match parsed_url.domain() {
            Some(d) => d.to_lowercase(),
            None => {
                debug!("No domain found in URL: {}", url);
                return Ok(None);
            }
        };

        // Skip obvious bad URLs
        if self.is_bad_url(&parsed_url, &domain) {
            debug!("Rejecting URL with known bad pattern: {}", url);
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
                match e.downcast_ref::<ValidationError>() {
                    Some(ValidationError::RateLimited { .. }) => {
                        // Rate limit errors are expected, don't log as warnings
                        debug!("Rate limited: {}", e);
                    }
                    Some(ValidationError::NetworkError(_)) => {
                        // Network errors might be transient
                        debug!("Network error validating {}: {}", url, e);
                    }
                    _ => {
                        // Other errors indicate invalid images
                        debug!("Failed to validate image {}: {}", url, e);
                    }
                }
                Ok(None)
            }
        }
    }

    fn is_bad_url(&self, url: &Url, domain: &str) -> bool {
        let path = url.path();

        // Check for known bad patterns in path
        if path.contains("favicon.ico")
            || path.contains("rss-to-nostr")
            || path.contains("default-avatar")
            || path.contains("placeholder")
        {
            return true;
        }

        // Check imgur - only allow direct image links from i.imgur.com
        if domain == "imgur.com" {
            // imgur.com without 'i.' subdomain is gallery/album links
            return true;
        }

        false
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
            .to_lowercase();

        // Check rate limit
        if let Err(wait_time) = self.rate_limit_manager.check_rate_limit(&domain).await {
            return Err(Box::new(ValidationError::RateLimited {
                domain,
                wait_time: Some(wait_time),
            }));
        }

        // First, try to get image info with a HEAD request
        let head_response = match self.client.head(url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                if is_network_error(&e) {
                    return Err(Box::new(ValidationError::NetworkError(e.to_string())));
                }
                // Fall back to range request if HEAD fails
                return self.get_info_with_range(url, &domain).await;
            }
        };

        // Handle rate limiting
        if head_response.status() == StatusCode::TOO_MANY_REQUESTS {
            self.update_rate_limiter_from_headers(&domain, &head_response)
                .await;
            return Err(Box::new(ValidationError::RateLimited {
                domain,
                wait_time: None,
            }));
        }

        // Check content type
        let content_type = head_response
            .headers()
            .get("content-type")
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        // Check if it's an SVG (needs full download)
        if content_type.contains("svg") || url.ends_with(".svg") {
            return self.download_and_analyze_svg(url, &domain).await;
        }

        // For raster images, use range request to get just the header
        self.get_info_with_range(url, &domain).await
    }

    async fn get_info_with_range(
        &self,
        url: &str,
        domain: &str,
    ) -> Result<ImageInfo, Box<dyn Error + Send + Sync>> {
        // Request only the first few KB for header parsing
        let range_header = format!("bytes=0-{}", self.max_header_bytes - 1);
        let response = match self
            .client
            .get(url)
            .header(RANGE, range_header)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                if is_network_error(&e) {
                    return Err(Box::new(ValidationError::NetworkError(e.to_string())));
                }
                return Err(Box::new(ValidationError::InvalidImage(e.to_string())));
            }
        };

        // Check status code
        if !response.status().is_success() && response.status() != StatusCode::PARTIAL_CONTENT {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                self.update_rate_limiter_from_headers(domain, &response)
                    .await;
                return Err(Box::new(ValidationError::RateLimited {
                    domain: domain.to_string(),
                    wait_time: None,
                }));
            }
            return Err(Box::new(ValidationError::InvalidImage(format!(
                "HTTP error: {}",
                response.status()
            ))));
        }

        // Get the header bytes
        let header_bytes = response.bytes().await?;

        // Try to parse dimensions from header using imagesize
        if let Ok(dimensions) = imagesize::blob_size(&header_bytes) {
            // We got dimensions, now guess format from the header
            if let Ok(format) = image::guess_format(&header_bytes) {
                let is_animated = self.check_animation_from_header(&header_bytes, &format);

                return Ok(ImageInfo {
                    width: dimensions.width as u32,
                    height: dimensions.height as u32,
                    format,
                    is_animated,
                });
            }
        }

        // If we can't get dimensions from header, fall back to full download
        self.download_and_analyze_full(url, domain).await
    }

    async fn download_and_analyze_svg(
        &self,
        url: &str,
        domain: &str,
    ) -> Result<ImageInfo, Box<dyn Error + Send + Sync>> {
        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                self.update_rate_limiter_from_headers(domain, &response)
                    .await;
                return Err(Box::new(ValidationError::RateLimited {
                    domain: domain.to_string(),
                    wait_time: None,
                }));
            }
            return Err(Box::new(ValidationError::InvalidImage(format!(
                "HTTP error: {}",
                response.status()
            ))));
        }

        let svg_bytes = response.bytes().await?;
        let svg_content = String::from_utf8_lossy(&svg_bytes);

        let (width, height) = self.parse_svg_dimensions(&svg_content)?;

        Ok(ImageInfo {
            width,
            height,
            format: ImageFormat::Png, // Use PNG as a placeholder format for SVG
            is_animated: false,
        })
    }

    async fn download_and_analyze_full(
        &self,
        url: &str,
        domain: &str,
    ) -> Result<ImageInfo, Box<dyn Error + Send + Sync>> {
        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                self.update_rate_limiter_from_headers(domain, &response)
                    .await;
                return Err(Box::new(ValidationError::RateLimited {
                    domain: domain.to_string(),
                    wait_time: None,
                }));
            }
            return Err(Box::new(ValidationError::InvalidImage(format!(
                "HTTP error: {}",
                response.status()
            ))));
        }

        // Limit download size to 10MB
        let max_size = 10 * 1024 * 1024;
        let content_length = response.content_length().unwrap_or(0);
        if content_length > max_size as u64 {
            return Err(Box::new(ValidationError::TooLarge(content_length as usize)));
        }

        let bytes = response.bytes().await?;
        if bytes.len() > max_size {
            return Err(Box::new(ValidationError::TooLarge(bytes.len())));
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

    fn check_animation_from_header(&self, header_bytes: &[u8], format: &ImageFormat) -> bool {
        match format {
            ImageFormat::Gif => {
                // Look for multiple Image Separator bytes in header
                let mut frame_count = 0;
                for i in 0..header_bytes.len().saturating_sub(1) {
                    if header_bytes[i] == 0x00 && header_bytes[i + 1] == 0x2C {
                        frame_count += 1;
                        if frame_count > 1 {
                            return true;
                        }
                    }
                }
                false
            }
            ImageFormat::WebP => {
                // Check VP8X chunk for animation flag
                header_bytes.len() > 30
                    && &header_bytes[12..16] == b"VP8X"
                    && (header_bytes[20] & 0x02) != 0
            }
            ImageFormat::Png => {
                // Look for acTL chunk (APNG)
                self.has_actl_chunk(header_bytes)
            }
            _ => false,
        }
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
            return Err(Box::new(ValidationError::InvalidImage(
                "Invalid data URL format".to_string(),
            )));
        }

        let data = if parts[0].contains("base64") {
            use base64::{engine::general_purpose, Engine as _};
            general_purpose::STANDARD.decode(parts[1])?
        } else {
            parts[1].as_bytes().to_vec()
        };

        // Try imagesize first
        if let Ok(dimensions) = imagesize::blob_size(&data) {
            // Get format from image crate
            let format = image::guess_format(&data).unwrap_or(ImageFormat::Png);
            return Ok(ImageInfo {
                width: dimensions.width as u32,
                height: dimensions.height as u32,
                format,
                is_animated: false, // Assume data URLs are not animated
            });
        }

        // Fall back to image crate
        let format = image::guess_format(&data)?;
        let (width, height) = self.get_dimensions(&data)?;

        Ok(ImageInfo {
            width,
            height,
            format,
            is_animated: false,
        })
    }

    fn get_dimensions(&self, data: &[u8]) -> Result<(u32, u32), Box<dyn Error + Send + Sync>> {
        // Try imagesize first (faster)
        if let Ok(dimensions) = imagesize::blob_size(data) {
            return Ok((dimensions.width as u32, dimensions.height as u32));
        }

        // Fall back to imageinfo
        match std::panic::catch_unwind(|| imageinfo::ImageInfo::from_raw_data(data)) {
            Ok(Ok(info)) => {
                return Ok((info.size.width as u32, info.size.height as u32));
            }
            Ok(Err(_)) => {}
            Err(_) => {
                warn!("imageinfo panicked while parsing image data");
            }
        }

        // Last resort: use image crate
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
                    if viewbox.w > 0.0 && viewbox.h > 0.0 {
                        return Ok((viewbox.w as u32, viewbox.h as u32));
                    }
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
                    if w > 0.0 {
                        width = Some(w as u32);
                    }
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
                    if h > 0.0 {
                        height = Some(h as u32);
                    }
                }
            }
        }

        match (width, height) {
            (Some(w), Some(h)) => Ok((w, h)),
            _ => {
                // If we can't find valid dimensions, reject the SVG
                Err(Box::new(ValidationError::InvalidImage(
                    "SVG missing valid width/height or viewBox attributes".to_string(),
                )))
            }
        }
    }
}

// Helper function to check if an error is a network error
fn is_network_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bad_url_patterns() {
        let validator = ProfileImageValidator::new(100, 100, false).unwrap();

        assert!(validator
            .check("https://example.com/favicon.ico")
            .await
            .unwrap()
            .is_none());
        assert!(validator
            .check("https://example.com/rss-to-nostr/image.png")
            .await
            .unwrap()
            .is_none());
        assert!(validator
            .check("https://imgur.com/gallery/abc123")
            .await
            .unwrap()
            .is_none());
    }
}
