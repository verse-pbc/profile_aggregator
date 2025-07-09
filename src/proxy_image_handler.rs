use axum::{body::Body, extract::Query, http::StatusCode, response::Response};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::{error, info, warn};

#[derive(Deserialize)]
pub struct ProxyQuery {
    url: String,
    secret: String,
}

pub async fn proxy_image_handler(Query(params): Query<ProxyQuery>) -> Response {
    info!("Proxy image request received for URL: {}", params.url);

    // Get the expected secret from environment
    let expected_secret = std::env::var("IMAGE_PROXY_SECRET").unwrap_or_else(|_| {
        warn!("IMAGE_PROXY_SECRET not set, using default");
        "default-proxy-secret".to_string()
    });

    // Verify secret
    if params.secret != expected_secret {
        info!("Proxy request rejected: invalid secret");
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::from("Unauthorized"))
            .unwrap();
    }

    // Parse and validate URL
    let target_url = match reqwest::Url::parse(&params.url) {
        Ok(url) => url,
        Err(e) => {
            info!("Proxy request rejected: invalid URL format - {}", e);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid URL"))
                .unwrap();
        }
    };

    // Validate that URL has a domain (basic check)
    let domain = match target_url.domain() {
        Some(d) => d,
        None => {
            info!("Proxy request rejected: URL has no domain");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid URL: no domain"))
                .unwrap();
        }
    };

    info!("Proxying request for domain '{}'", domain);

    // Create HTTP client with browser-like headers
    let client = match Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Internal server error"))
                .unwrap();
        }
    };

    // Fetch the image
    info!("Fetching image from: {}", params.url);

    let response = match client
        .get(params.url.clone())
        .header("Accept", "image/webp,image/apng,image/*,*/*;q=0.8")
        .header("Accept-Encoding", "gzip, deflate, br")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Sec-Fetch-Dest", "image")
        .header("Sec-Fetch-Mode", "no-cors")
        .header("Sec-Fetch-Site", "cross-site")
        .send()
        .await
    {
        Ok(resp) => {
            info!("Received response with status: {}", resp.status());
            resp
        }
        Err(e) => {
            error!("Failed to fetch image from {}: {}", params.url, e);
            info!("Proxy request failed: network error - {}", e);
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Failed to fetch image"))
                .unwrap();
        }
    };

    // Check response status
    if !response.status().is_success() {
        info!(
            "Proxy request failed: upstream returned error status {}",
            response.status()
        );
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from(format!("Upstream error: {}", response.status())))
            .unwrap();
    }

    // Get content type and content length before consuming response
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/png")
        .to_string();

    // Check content length (max 5MB)
    if let Some(content_length) = response.content_length() {
        info!("Content-Length: {} bytes", content_length);
        if content_length > 5 * 1024 * 1024 {
            info!(
                "Proxy request rejected: image too large ({} bytes)",
                content_length
            );
            return Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .body(Body::from("Image too large"))
                .unwrap();
        }
    } else {
        info!("No Content-Length header in response");
    }

    info!("Reading response body...");

    // Stream the response back
    match response.bytes().await {
        Ok(bytes) => {
            info!(
                "Successfully fetched {} bytes, returning to client",
                bytes.len()
            );
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", content_type)
                .header("Cache-Control", "public, max-age=31536000, immutable")
                .body(Body::from(bytes))
                .unwrap()
        }
        Err(e) => {
            error!("Failed to read image bytes: {}", e);
            info!("Proxy request failed: error reading response body - {}", e);
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Failed to read image"))
                .unwrap()
        }
    }
}
