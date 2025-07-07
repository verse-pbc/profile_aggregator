use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use nostr_lmdb::NostrLMDB;
use nostr_sdk::prelude::*;
use reqwest::Client;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "facesync",
    version = "0.1.0",
    about = "Sync profile avatars to CloudFlare R2 cache"
)]
struct Args {
    /// Path to the LMDB database directory
    #[arg(short, long, default_value = "./data/profile_aggregator.db")]
    database: PathBuf,

    /// Base URL for the CloudFlare worker
    #[arg(short, long, default_value = "https://face.yestr.social")]
    base_url: String,

    /// Number of concurrent requests
    #[arg(short, long, default_value = "10")]
    concurrency: usize,

    /// Request timeout in seconds
    #[arg(short, long, default_value = "30")]
    timeout: u64,

    /// Continue on error
    #[arg(long)]
    skip_errors: bool,
}

fn setup_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,facesync=debug"));

    fmt()
        .with_env_filter(env_filter)
        .with_timer(fmt::time::SystemTime)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_file(false)
        .with_line_number(false)
        .with_level(true)
        .init();
}

async fn sync_profiles(
    db: NostrLMDB,
    base_url: String,
    concurrency: usize,
    timeout: Duration,
    skip_errors: bool,
) -> Result<()> {
    info!("Fetching all metadata events (kind 0) from database...");

    // Query for all kind 0 events
    let filter = Filter::new().kind(Kind::Metadata);
    let events = db
        .query(filter)
        .await
        .context("Failed to query metadata events")?;

    info!("Found {} profiles to sync", events.len());

    if events.is_empty() {
        info!("No profiles found in database");
        return Ok(());
    }

    info!(
        "Settings: GET requests, {}s timeout, {}",
        timeout.as_secs(),
        if skip_errors {
            "continue on errors"
        } else {
            "stop on first error"
        }
    );
    info!("Starting sync with {} concurrent requests...", concurrency);

    let client = Client::builder()
        .timeout(timeout)
        .build()
        .context("Failed to build HTTP client")?;

    let progress_bar = ProgressBar::new(events.len() as u64);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} profiles ({percent}%) ({eta})",
            )?
            .progress_chars("#>-"),
    );

    let mut success_count = 0u64;
    let mut error_count = 0u64;
    let total_count = events.len();

    // Process profiles in batches
    let chunks: Vec<Vec<Event>> = events
        .into_iter()
        .collect::<Vec<_>>()
        .chunks(concurrency)
        .map(|chunk| chunk.to_vec())
        .collect();

    for chunk in chunks {
        let mut handles = vec![];

        for event in chunk {
            let pubkey = event.pubkey.to_hex();
            let url = format!("{base_url}/avatar/{pubkey}");
            let client = client.clone();

            let handle = tokio::spawn(async move {
                // Just send the request and check if it was sent successfully
                // Don't wait for or process the response body
                match client.get(&url).send().await {
                    Ok(_) => {
                        // Request sent successfully, we don't care about the response
                        Ok((pubkey, true))
                    }
                    Err(e) => Err(anyhow::anyhow!(
                        "Failed to send request for {}: {}",
                        pubkey,
                        e
                    )),
                }
            });

            handles.push(handle);
        }

        // Wait for all requests in this batch to complete
        for handle in handles {
            match handle.await {
                Ok(Ok((pubkey, _))) => {
                    success_count += 1;
                    let percent = (success_count + error_count) as f64 / total_count as f64 * 100.0;
                    progress_bar.set_message(format!("✓ {}", &pubkey[..8]));
                    progress_bar.inc(1);
                    // Log without interfering with progress bar
                    progress_bar.println(format!("✓ [{percent:.1}%] Synced avatar for {pubkey}"));
                }
                Ok(Err(e)) => {
                    error_count += 1;
                    progress_bar.inc(1);
                    if skip_errors {
                        progress_bar.println(format!("⚠ {e}"));
                    } else {
                        progress_bar.println(format!("✗ {e}"));
                        progress_bar.finish_with_message("Failed");
                        return Err(e);
                    }
                }
                Err(e) => {
                    error_count += 1;
                    progress_bar.inc(1);
                    let err = anyhow::anyhow!("Task panicked: {}", e);
                    if skip_errors {
                        progress_bar.println(format!("⚠ {err}"));
                    } else {
                        progress_bar.println(format!("✗ {err}"));
                        progress_bar.finish_with_message("Failed");
                        return Err(err);
                    }
                }
            }
        }
    }

    progress_bar.finish_with_message("Sync complete");

    info!(
        "Avatar sync completed: {} successful, {} errors",
        success_count, error_count
    );

    if error_count > 0 && !skip_errors {
        return Err(anyhow::anyhow!(
            "Sync completed with {} errors",
            error_count
        ));
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_tracing();
    let args = Args::parse();

    info!("Opening database at {:?}", args.database);
    let database = NostrLMDB::open(&args.database)
        .with_context(|| format!("Failed to open database at {:?}", args.database))?;

    sync_profiles(
        database,
        args.base_url,
        args.concurrency,
        Duration::from_secs(args.timeout),
        args.skip_errors,
    )
    .await
}
