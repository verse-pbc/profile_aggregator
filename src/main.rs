use anyhow::Result;
use axum::{
    extract::{ConnectInfo, Query},
    http::HeaderMap,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use nostr_relay_builder::{CryptoWorker, RelayBuilder, RelayConfig, RelayDatabase, RelayInfo};
use nostr_sdk::prelude::*;
use profile_aggregator::{
    ProfileAggregationConfig, ProfileAggregationService, ProfileQualityFilter,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Deserialize)]
struct RandomQuery {
    #[serde(default = "default_count")]
    count: usize,
}

fn default_count() -> usize {
    12
}

#[derive(Serialize)]
struct ProfileWithRelays {
    profile: Event,
    #[serde(skip_serializing_if = "Option::is_none")]
    relay_list: Option<Event>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dm_relay_list: Option<Event>,
}

async fn random_profiles_handler_impl(
    database: Arc<RelayDatabase>,
    params: RandomQuery,
    headers: HeaderMap,
) -> Response {
    // Cap the count at 500
    let count = params.count.min(500);

    // Get current timestamp and create random time windows
    let now = Timestamp::now();
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::from_entropy();

    // Collect random profiles
    let mut profiles = Vec::new();
    let mut attempts = 0;
    let max_attempts = count * 10; // Try up to 10x to get enough profiles

    while profiles.len() < count && attempts < max_attempts {
        attempts += 1;

        // Generate random time window (looking back up to ~6 months)
        let random_seconds_ago = rng.gen_range(0..15_552_000u64); // 180 days in seconds
        let window_start = Timestamp::from(now.as_u64().saturating_sub(random_seconds_ago));
        let window_size = rng.gen_range(3600..86400); // 1 hour to 1 day window
        let window_end = Timestamp::from(window_start.as_u64() + window_size);

        let filter = Filter::new()
            .kind(Kind::Metadata)
            .since(window_start)
            .until(window_end)
            .limit(count - profiles.len());

        let scope = nostr_lmdb::Scope::Default;
        match database.query(vec![filter], &scope).await {
            Ok(events) => {
                for event in events {
                    if !profiles.iter().any(|p: &Event| p.pubkey == event.pubkey) {
                        profiles.push(event);
                        if profiles.len() >= count {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to query profiles: {}", e);
            }
        }
    }

    // Now fetch relay lists for each profile
    let mut results = Vec::new();
    for profile in profiles {
        let relay_filter = Filter::new()
            .author(profile.pubkey)
            .kinds(vec![Kind::Custom(10002), Kind::Custom(10050)]);

        let mut relay_list = None;
        let mut dm_relay_list = None;

        let scope = nostr_lmdb::Scope::Default;
        if let Ok(relay_events) = database.query(vec![relay_filter], &scope).await {
            for event in relay_events {
                match event.kind.as_u16() {
                    10002 => relay_list = Some(event),
                    10050 => dm_relay_list = Some(event),
                    _ => {}
                }
            }
        }

        results.push(ProfileWithRelays {
            profile,
            relay_list,
            dm_relay_list,
        });
    }

    // Check Accept header to determine response type
    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if accept.contains("text/html") {
        // Return HTML response
        Html(render_profiles_html(&results)).into_response()
    } else {
        // Return JSON response
        Json(results).into_response()
    }
}

fn render_profiles_html(profiles: &[ProfileWithRelays]) -> String {
    let profiles_json = serde_json::to_string(profiles).unwrap_or_default();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Random Nostr Profiles</title>
    <style>
        * {{
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }}
        
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, Cantarell, sans-serif;
            background: #0a0a0a;
            color: #e0e0e0;
            line-height: 1.6;
        }}
        
        .container {{
            max-width: 1200px;
            margin: 0 auto;
            padding: 20px;
        }}
        
        h1 {{
            text-align: center;
            margin-bottom: 40px;
            color: #fff;
            font-size: 2.5rem;
        }}
        
        .profiles-grid {{
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(350px, 1fr));
            gap: 30px;
        }}
        
        .profile-card {{
            background: #1a1a1a;
            border-radius: 12px;
            padding: 24px;
            border: 1px solid #333;
            transition: transform 0.2s, box-shadow 0.2s;
            cursor: pointer;
            position: relative;
        }}
        
        .profile-card:hover {{
            transform: translateY(-4px);
            box-shadow: 0 8px 32px rgba(140, 80, 255, 0.1);
            border-color: #8c50ff;
        }}
        
        .profile-header {{
            display: flex;
            align-items: center;
            margin-bottom: 16px;
        }}
        
        .profile-image {{
            width: 80px;
            height: 80px;
            border-radius: 50%;
            object-fit: cover;
            margin-right: 16px;
            border: 3px solid #333;
        }}
        
        .profile-info {{
            flex: 1;
        }}
        
        .profile-name {{
            font-size: 1.25rem;
            font-weight: 600;
            color: #fff;
            margin-bottom: 4px;
        }}
        
        .profile-pubkey {{
            font-size: 0.875rem;
            color: #666;
            font-family: monospace;
            word-break: break-all;
        }}
        
        .profile-updated {{
            font-size: 0.75rem;
            color: #555;
            margin-top: 4px;
        }}
        
        .profile-bio {{
            margin-bottom: 16px;
            color: #ccc;
            overflow: hidden;
            display: -webkit-box;
            -webkit-line-clamp: 3;
            -webkit-box-orient: vertical;
        }}
        
        .profile-metadata {{
            margin-bottom: 16px;
            display: flex;
            flex-direction: column;
            gap: 8px;
        }}
        
        .metadata-item {{
            font-size: 0.875rem;
            color: #999;
            display: flex;
            align-items: center;
            gap: 8px;
        }}
        
        .metadata-icon {{
            flex-shrink: 0;
            font-size: 1rem;
        }}
        
        .metadata-item a {{
            color: #8c50ff;
            text-decoration: none;
            word-break: break-all;
            transition: color 0.2s;
        }}
        
        .metadata-item a:hover {{
            color: #a970ff;
            text-decoration: underline;
        }}
        
        .relay-section {{
            margin-top: 16px;
            padding-top: 16px;
            border-top: 1px solid #333;
        }}
        
        .relay-title {{
            font-size: 0.875rem;
            color: #8c50ff;
            font-weight: 600;
            margin-bottom: 8px;
        }}
        
        .relay-list {{
            font-size: 0.75rem;
            color: #999;
            font-family: monospace;
            max-height: 100px;
            overflow-y: auto;
        }}
        
        .relay-item {{
            display: flex;
            justify-content: space-between;
            align-items: center;
            padding: 2px 0;
        }}
        
        .relay-url {{
            flex: 1;
            overflow: hidden;
            text-overflow: ellipsis;
            white-space: nowrap;
        }}
        
        .relay-marker {{
            color: #666;
            font-size: 0.7rem;
            margin-left: 8px;
            flex-shrink: 0;
        }}
        
        .relay-list::-webkit-scrollbar {{
            width: 4px;
        }}
        
        .relay-list::-webkit-scrollbar-track {{
            background: #333;
        }}
        
        .relay-list::-webkit-scrollbar-thumb {{
            background: #666;
            border-radius: 2px;
        }}
        
        .no-relays {{
            color: #666;
            font-style: italic;
            font-size: 0.75rem;
        }}
        
        .loading {{
            text-align: center;
            padding: 40px;
            color: #666;
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>Random Nostr Profiles</h1>
        <div id="profiles-container" class="profiles-grid">
            <div class="loading">Loading profiles...</div>
        </div>
    </div>
    
    <script src="https://unpkg.com/nostr-tools@2.5.2/lib/nostr.bundle.js"></script>
    <script>
        const {{ nip19 }} = window.NostrTools;
        const profilesData = {profiles_json};
        
        function truncateNpub(npub) {{
            return npub.slice(0, 12) + '...' + npub.slice(-8);
        }}
        
        function truncateUrl(url) {{
            if (url.length <= 30) return url;
            return url.slice(0, 25) + '...' + url.slice(-5);
        }}
        
        function formatWebsite(website) {{
            // Remove protocol for display
            return website.replace(/^https?:\/\//, '').replace(/\/$/, '');
        }}
        
        function formatTimestamp(timestamp) {{
            const date = new Date(timestamp * 1000);
            const now = new Date();
            const diffMs = now - date;
            const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));
            
            if (diffDays === 0) {{
                return 'Updated today';
            }} else if (diffDays === 1) {{
                return 'Updated yesterday';
            }} else if (diffDays < 7) {{
                return `Updated ${{diffDays}} days ago`;
            }} else if (diffDays < 30) {{
                const weeks = Math.floor(diffDays / 7);
                return `Updated ${{weeks}} week${{weeks > 1 ? 's' : ''}} ago`;
            }} else if (diffDays < 365) {{
                const months = Math.floor(diffDays / 30);
                return `Updated ${{months}} month${{months > 1 ? 's' : ''}} ago`;
            }} else {{
                const years = Math.floor(diffDays / 365);
                return `Updated ${{years}} year${{years > 1 ? 's' : ''}} ago`;
            }}
        }}
        
        function parseRelays(event) {{
            if (!event) return [];
            try {{
                const relays = [];
                for (const tag of event.tags) {{
                    if (tag[0] === 'r' && tag[1]) {{
                        const url = tag[1];
                        const marker = tag[2]; // 'read', 'write', or undefined (both)
                        relays.push({{ url, marker }});
                    }}
                }}
                return relays;
            }} catch (e) {{
                return [];
            }}
        }}
        
        function renderProfiles() {{
            const container = document.getElementById('profiles-container');
            container.innerHTML = '';
            
            profilesData.forEach(item => {{
                const profile = item.profile;
                let metadata;
                try {{
                    metadata = JSON.parse(profile.content);
                }} catch (e) {{
                    metadata = {{}};
                }}
                
                const name = metadata.display_name || metadata.name || 'Anonymous';
                const picture = metadata.picture || '';
                const about = metadata.about || '';
                const website = metadata.website || '';
                const nip05 = metadata.nip05 || '';
                const lud16 = metadata.lud16 || '';
                const npub = nip19.npubEncode(profile.pubkey);
                
                const relays = parseRelays(item.relay_list);
                const dmRelays = parseRelays(item.dm_relay_list);
                
                const card = document.createElement('div');
                card.className = 'profile-card';
                card.onclick = () => window.open(`https://njump.me/${{npub}}`, '_blank');
                
                card.innerHTML = `
                    <div class="profile-header">
                        ${{picture ? `<img src="${{picture}}" alt="${{name}}" class="profile-image" onerror="this.style.display='none'">` : ''}}
                        <div class="profile-info">
                            <div class="profile-name">${{name}}</div>
                            <div class="profile-pubkey">${{truncateNpub(npub)}}</div>
                            <div class="profile-updated">${{formatTimestamp(profile.created_at)}}</div>
                        </div>
                    </div>
                    ${{about ? `<div class="profile-bio">${{about}}</div>` : ''}}
                    <div class="profile-metadata">
                        ${{nip05 ? `<div class="metadata-item"><span class="metadata-icon">✓</span> ${{nip05}}</div>` : ''}}
                        ${{website ? `<div class="metadata-item"><span class="metadata-icon">🔗</span> <a href="${{website.startsWith('http') ? website : 'https://' + website}}" target="_blank" rel="noopener noreferrer" onclick="event.stopPropagation()">${{formatWebsite(website)}}</a></div>` : ''}}
                        ${{lud16 ? `<div class="metadata-item"><span class="metadata-icon">⚡</span> ${{lud16}}</div>` : ''}}
                    </div>
                    ${{relays.length > 0 ? `
                        <div class="relay-section">
                            <div class="relay-title">Relay List (NIP-65)</div>
                            <div class="relay-list">${{relays.map(r => {{
                                const marker = r.marker ? ` [${{r.marker}}]` : ' [read/write]';
                                return `<div class="relay-item"><span class="relay-url">${{r.url}}</span><span class="relay-marker">${{marker}}</span></div>`;
                            }}).join('')}}</div>
                        </div>
                    ` : '<div class="no-relays">No relay list</div>'}}
                    ${{dmRelays.length > 0 ? `
                        <div class="relay-section">
                            <div class="relay-title">DM Inbox Relays (NIP-17)</div>
                            <div class="relay-list">${{dmRelays.map(r => r.url).join('<br>')}}</div>
                        </div>
                    ` : ''}}
                `;
                
                container.appendChild(card);
            }});
        }}
        
        // Render profiles on load
        renderProfiles();
    </script>
</body>
</html>"#
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables
    dotenv::dotenv().ok();

    // Setup logging
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,profile_aggregator=debug,nostr_relay_builder=debug")
    });
    fmt().with_env_filter(env_filter).with_target(true).init();

    // Load relay keys from environment or use default dev key
    let relay_secret = std::env::var("RELAY_SECRET_KEY").unwrap_or_else(|_| {
        "339e1ab1f59eb304b8cb5202eddcc437ff699fc523161b6e2c222590cccb3b84".to_string()
    });

    let keys = Keys::parse(&relay_secret)?;
    println!("🔑 Relay public key: {}", keys.public_key());

    // Create the crypto worker and database
    let cancellation_token = CancellationToken::new();
    let task_tracker = TaskTracker::new();
    let crypto_sender = CryptoWorker::spawn(Arc::new(keys.clone()), &task_tracker);
    let database_path = std::env::var("DATABASE_PATH")
        .unwrap_or_else(|_| "./data/profile_aggregator.db".to_string());

    let database = Arc::new(RelayDatabase::new(&database_path, crypto_sender)?);

    // Configure the relay
    let relay_url =
        std::env::var("RELAY_URL").unwrap_or_else(|_| "ws://localhost:8080".to_string());
    let config = RelayConfig::new(&relay_url, database.clone(), keys);

    // Get discovery relay URLs
    let discovery_relay_urls = vec![std::env::var("DISCOVERY_RELAY_URL")
        .unwrap_or_else(|_| "wss://relay.nos.social".to_string())];

    // Configure profile aggregation service to only fetch metadata events
    let page_size: usize = std::env::var("PAGE_SIZE")
        .unwrap_or_else(|_| "500".to_string())
        .parse()
        .unwrap_or(500);

    let initial_backoff_secs: u64 = std::env::var("INITIAL_BACKOFF_SECS")
        .unwrap_or_else(|_| "2".to_string())
        .parse()
        .unwrap_or(2);

    let max_backoff_secs: u64 = std::env::var("MAX_BACKOFF_SECS")
        .unwrap_or_else(|_| "300".to_string())
        .parse()
        .unwrap_or(300);

    let state_file =
        std::env::var("STATE_FILE").unwrap_or_else(|_| "./data/aggregation_state.json".to_string());

    let aggregation_config = ProfileAggregationConfig {
        relay_urls: discovery_relay_urls.clone(),
        filters: vec![Filter::new().kind(Kind::Metadata)],
        page_size,
        state_file: PathBuf::from(state_file.clone()),
        initial_backoff: Duration::from_secs(initial_backoff_secs),
        max_backoff: Duration::from_secs(max_backoff_secs),
    };

    // Create shared filter for both WebSocket and aggregation service
    let filter = Arc::new(ProfileQualityFilter::new(database.clone()));

    // Both services use the same filter
    let aggregation_service =
        ProfileAggregationService::new(aggregation_config, filter.clone(), database.clone())
            .await
            .expect("Failed to create aggregation service");

    // Create relay info for NIP-11
    let relay_info = RelayInfo {
        name: "Profile Aggregator Relay".to_string(),
        description: "A relay that aggregates and filters user profiles".to_string(),
        pubkey: config.keys.public_key().to_hex(),
        contact: std::env::var("RELAY_CONTACT")
            .unwrap_or_else(|_| "admin@relay.example".to_string()),
        supported_nips: vec![1, 11],
        software: "profile_aggregator".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        icon: None,
    };

    // Build the WebSocket server without default HTML
    let ws_handler = RelayBuilder::new(config.clone())
        .without_html()
        .build_axum_handler(filter.as_ref().clone(), relay_info.clone())
        .await?;

    // Create custom root handler that combines WebSocket and HTML
    let root_handler = {
        let relay_info = relay_info.clone();
        let relay_url = config.relay_url.clone();
        let ws_handler = ws_handler.clone();
        move |ws: Option<axum::extract::WebSocketUpgrade>,
              ConnectInfo(addr): ConnectInfo<SocketAddr>,
              headers: HeaderMap| {
            let info = relay_info.clone();
            let url = relay_url.clone();
            let ws_h = ws_handler.clone();
            async move {
                // If it's a WebSocket upgrade, delegate to the WebSocket handler
                if let Some(ws) = ws {
                    return ws_h(Some(ws), ConnectInfo(addr), headers).await;
                }

                // Check if client wants JSON (NIP-11)
                if let Some(accept) = headers.get("accept") {
                    if let Ok(accept_str) = accept.to_str() {
                        if accept_str.contains("application/nostr+json") {
                            return Json(&info).into_response();
                        }
                    }
                }

                // Otherwise return HTML
                let html = format!(
                    r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{}</title>
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            line-height: 1.6;
            color: #333;
            max-width: 800px;
            margin: 0 auto;
            padding: 20px;
            background: #f5f5f5;
        }}
        .container {{
            background: white;
            padding: 30px;
            border-radius: 10px;
            box-shadow: 0 2px 10px rgba(0,0,0,0.1);
        }}
        h1 {{
            color: #2c3e50;
            border-bottom: 2px solid #3498db;
            padding-bottom: 10px;
        }}
        .info-grid {{
            display: grid;
            grid-template-columns: auto 1fr;
            gap: 15px;
            margin: 20px 0;
        }}
        .label {{
            font-weight: bold;
            color: #555;
        }}
        .value {{
            color: #333;
            word-break: break-all;
        }}
        .features {{
            margin-top: 30px;
            padding: 20px;
            background: #f8f9fa;
            border-radius: 8px;
        }}
        .features h2 {{
            margin-top: 0;
            color: #2c3e50;
        }}
        .link-button {{
            display: inline-block;
            margin: 10px 10px 10px 0;
            padding: 10px 20px;
            background: #3498db;
            color: white;
            text-decoration: none;
            border-radius: 5px;
            transition: background 0.3s;
        }}
        .link-button:hover {{
            background: #2980b9;
        }}
        code {{
            background: #f1f1f1;
            padding: 2px 5px;
            border-radius: 3px;
            font-family: monospace;
        }}
        .supported-nips {{
            display: flex;
            gap: 10px;
            flex-wrap: wrap;
        }}
        .nip {{
            background: #e8f4f8;
            padding: 5px 10px;
            border-radius: 5px;
            font-size: 0.9em;
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>{}</h1>
        <p>{}</p>
        
        <div class="info-grid">
            <div class="label">WebSocket:</div>
            <div class="value">{}</div>
            
            <div class="label">Pubkey:</div>
            <div class="value">{}</div>
            
            <div class="label">Contact:</div>
            <div class="value">{}</div>
            
            <div class="label">Software:</div>
            <div class="value">{} v{}</div>
            
            <div class="label">Supported NIPs:</div>
            <div class="value">
                <div class="supported-nips">
                    {}
                </div>
            </div>
        </div>
        
        <div class="features">
            <h2>Features</h2>
            <p>This relay aggregates and validates high-quality Nostr user profiles with the following requirements:</p>
            <ul>
                <li>Must have a name and bio</li>
                <li>Profile picture must be at least 300x600 pixels</li>
                <li>User must have published at least one text note</li>
                <li>Excludes bridge accounts (mostr, ActivityPub)</li>
            </ul>
            
            <h3>API Endpoints</h3>
            <p><strong>/random</strong> - Get random validated profiles</p>
            <ul>
                <li>Query parameter: <code>?count=N</code> (default: 12, max: 500)</li>
                <li>Returns JSON by default</li>
                <li>Returns HTML gallery if Accept header contains "text/html"</li>
            </ul>
            
            <div style="margin-top: 20px;">
                <a href="/random" class="link-button">View Random Profiles</a>
                <a href="/random?count=20" class="link-button">View 20 Random Profiles</a>
            </div>
            
            <h3>Example API Usage</h3>
            <pre style="background: #f1f1f1; padding: 10px; border-radius: 5px; overflow-x: auto;">
# Get JSON response (default)
curl https://relay.yestr.social/random

# Get 20 profiles as JSON
curl https://relay.yestr.social/random?count=20

# Get HTML response
curl -H "Accept: text/html" https://relay.yestr.social/random</pre>
        </div>
    </div>
</body>
</html>"#,
                    info.name,
                    info.name,
                    info.description,
                    url.replace("ws://", "wss://"),
                    info.pubkey,
                    info.contact,
                    info.software,
                    info.version,
                    info.supported_nips
                        .iter()
                        .map(|nip| format!(r#"<span class="nip">NIP-{:02}</span>"#, nip))
                        .collect::<Vec<_>>()
                        .join("")
                );

                Html(html).into_response()
            }
        }
    };

    // Create HTTP server
    // Clone database for the handler
    let db_for_handler = database.clone();

    // Create the handler as a closure
    let handler = move |Query(params): Query<RandomQuery>, headers: HeaderMap| {
        let db = db_for_handler.clone();
        async move { random_profiles_handler_impl(db, params, headers).await }
    };

    // Create router with both WebSocket and HTML handlers
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/health", get(|| async { "OK" }))
        .route("/random", get(handler));

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind_addr.parse()?;

    // Spawn the aggregation service
    let aggregation_service_token = cancellation_token.clone();
    let aggregation_handle = tokio::spawn(async move {
        info!("🔄 Starting profile aggregation service...");
        tokio::select! {
            result = aggregation_service.run() => {
                if let Err(e) = result {
                    error!("Profile aggregation service error: {}", e);
                }
            }
            _ = aggregation_service_token.cancelled() => {
                info!("Profile aggregation service cancelled");
            }
        }
    });

    println!("\nProfile Aggregator starting");
    println!("WebSocket: ws://{}", addr);
    println!("\nFetching user profiles (kind 0)");
    println!("\nDiscovery relay: {}", discovery_relay_urls.join(", "));
    println!("\nProfile requirements:");
    println!("- Name: display_name or name field");
    println!("- Bio: non-empty about field");
    println!("- Picture: valid URL, min 300x600px");
    println!("- Verified: published text note via outbox relays");
    println!("- Excludes: bridges, mostr accounts, profiles with fields array");
    println!("\nConfiguration:");
    println!("- Page size: {} events", page_size);
    println!("- Backoff: {}s-{}s", initial_backoff_secs, max_backoff_secs);
    println!("- Database: {}", database_path);
    println!("- State: {}", state_file);

    // Handle shutdown signal
    let shutdown_token = cancellation_token.clone();
    let shutdown_signal = async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        info!("Shutdown signal received, stopping services...");
        shutdown_token.cancel();
    };

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Run server with graceful shutdown
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal);

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                error!("Server error: {}", e);
            }
        }
        _ = aggregation_handle => {
            info!("Profile aggregation service stopped");
        }
        _ = cancellation_token.cancelled() => {
            info!("Cancellation requested");
        }
    }

    info!("Server stopped");

    Ok(())
}
