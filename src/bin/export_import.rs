use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::TryFutureExt;
use indicatif::{ProgressBar, ProgressStyle};
use nostr_lmdb::{NostrLMDB, Scope};
use nostr_sdk::prelude::*;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "export-import",
    version = "0.1.0",
    about = "Export and import Nostr events from LMDB database by scope"
)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Export all events from the database to JSONL files by scope
    Export {
        /// Path to the LMDB database directory
        #[arg(short, long)]
        db: PathBuf,

        /// Output directory for exported JSONL files
        #[arg(short, long)]
        output: PathBuf,

        /// Overwrite existing files in output directory
        #[arg(long)]
        force: bool,

        /// Include event count in filename (e.g., scope_default_1234.jsonl)
        #[arg(long)]
        include_count: bool,
    },
    /// Import events from JSONL files back into the database
    Import {
        /// Path to the LMDB database directory
        #[arg(short, long)]
        db: PathBuf,

        /// Input directory containing JSONL files to import
        #[arg(short, long)]
        input: PathBuf,

        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,

        /// Continue on error (skip invalid events)
        #[arg(long)]
        skip_errors: bool,
    },
}

fn setup_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,export_import=debug"));

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

fn scope_to_filename(scope: &Scope, include_count: bool, count: Option<u64>) -> String {
    let base = match scope {
        Scope::Default => "scope_default".to_string(),
        Scope::Named { name, .. } => format!("scope_{}", sanitize_filename(name)),
    };

    if include_count && count.is_some() {
        format!("{}_{}.jsonl", base, count.unwrap())
    } else {
        format!("{base}.jsonl")
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_scope_from_filename(filename: &str) -> Option<Scope> {
    if !filename.ends_with(".jsonl") {
        return None;
    }

    let name = filename.trim_end_matches(".jsonl");

    // Handle filenames with counts (e.g., scope_default_1234.jsonl)
    let parts: Vec<&str> = name.split('_').collect();

    if parts.len() < 2 || parts[0] != "scope" {
        return None;
    }

    if parts[1] == "default" {
        Some(Scope::Default)
    } else {
        // Join all parts except the first one and potentially the last one if it's a number
        let end_index = if parts.len() > 2 && parts.last().unwrap().parse::<u64>().is_ok() {
            parts.len() - 1
        } else {
            parts.len()
        };

        let scope_name = parts[1..end_index].join("_");
        Scope::named(&scope_name).ok()
    }
}

async fn export_scope(
    db: &Arc<NostrLMDB>,
    scope: &Scope,
    output_path: &Path,
    include_count: bool,
) -> Result<u64> {
    // Get scoped view of the database
    let scoped_db = db
        .scoped(scope)
        .map_err(|e| anyhow::anyhow!("Failed to get scoped view: {}", e))?;

    let filter = Filter::new();
    let events = scoped_db
        .query(filter)
        .map_err(|e| anyhow::anyhow!("Failed to query events: {}", e))
        .await?;
    let count = events.len() as u64;

    let filename = scope_to_filename(scope, include_count, Some(count));
    let file_path = output_path.join(&filename);

    let file = File::create(&file_path)
        .with_context(|| format!("Failed to create file: {file_path:?}"))?;
    let mut writer = BufWriter::new(file);

    for event in events {
        let json = event.as_json();
        writeln!(writer, "{json}")?;
    }

    writer.flush()?;
    info!("Exported {} events to {}", count, filename);
    Ok(count)
}

async fn export_database(
    db: Arc<NostrLMDB>,
    output_dir: PathBuf,
    force: bool,
    include_count: bool,
) -> Result<()> {
    // Create output directory if it doesn't exist
    if !output_dir.exists() {
        fs::create_dir_all(&output_dir)
            .with_context(|| format!("Failed to create output directory: {output_dir:?}"))?;
    }

    // Check if directory is empty or force flag is set
    if !force && output_dir.read_dir()?.next().is_some() {
        return Err(anyhow::anyhow!(
            "Output directory is not empty. Use --force to overwrite existing files."
        ));
    }

    info!("Fetching all scopes from database...");
    let scopes = db.list_scopes()?;
    info!("Found {} scopes to export", scopes.len());

    let progress_bar = ProgressBar::new(scopes.len() as u64);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} scopes ({eta})")?
            .progress_chars("#>-"),
    );

    let mut total_events = 0u64;
    let mut exported_scopes = 0u64;

    for scope in scopes {
        match export_scope(&db, &scope, &output_dir, include_count).await {
            Ok(count) => {
                total_events += count;
                exported_scopes += 1;
            }
            Err(e) => {
                error!("Failed to export scope {:?}: {}", scope, e);
            }
        }
        progress_bar.inc(1);
    }

    progress_bar.finish_with_message("Export complete");

    info!(
        "Successfully exported {} events from {} scopes to {:?}",
        total_events, exported_scopes, output_dir
    );

    Ok(())
}

async fn import_scope(
    db: &Arc<NostrLMDB>,
    scope: &Scope,
    file_path: &Path,
    skip_errors: bool,
) -> Result<(u64, u64)> {
    let file =
        File::open(file_path).with_context(|| format!("Failed to open file: {file_path:?}"))?;
    let reader = BufReader::new(file);

    let mut imported = 0u64;
    let mut errors = 0u64;
    let mut events_batch = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        match Event::from_json(&line) {
            Ok(event) => {
                events_batch.push(event);

                // Process in batches of 100 events
                if events_batch.len() >= 100 {
                    match save_events_batch(db, scope, &events_batch).await {
                        Ok(count) => imported += count,
                        Err(e) => {
                            errors += events_batch.len() as u64;
                            if !skip_errors {
                                return Err(anyhow::anyhow!(
                                    "Failed to import batch ending at line {}: {}",
                                    line_num + 1,
                                    e
                                ));
                            }
                        }
                    }
                    events_batch.clear();
                }
            }
            Err(e) => {
                errors += 1;
                if skip_errors {
                    warn!(
                        "Failed to parse event at line {} in {:?}: {}",
                        line_num + 1,
                        file_path,
                        e
                    );
                } else {
                    return Err(anyhow::anyhow!(
                        "Failed to parse event at line {} in {:?}: {}",
                        line_num + 1,
                        file_path,
                        e
                    ));
                }
            }
        }
    }

    // Process remaining events
    if !events_batch.is_empty() {
        match save_events_batch(db, scope, &events_batch).await {
            Ok(count) => imported += count,
            Err(e) => {
                errors += events_batch.len() as u64;
                if !skip_errors {
                    return Err(anyhow::anyhow!("Failed to import final batch: {}", e));
                }
            }
        }
    }

    Ok((imported, errors))
}

async fn save_events_batch(db: &Arc<NostrLMDB>, scope: &Scope, events: &[Event]) -> Result<u64> {
    // Get scoped view of the database
    let scoped_db = db
        .scoped(scope)
        .map_err(|e| anyhow::anyhow!("Failed to get scoped view: {}", e))?;

    // Save events one by one since save_events doesn't exist on ScopedView
    let mut saved_count = 0u64;
    for event in events {
        match scoped_db.save_event(event).await {
            Ok(status) => {
                if status.is_success() {
                    saved_count += 1;
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to save event: {}", e));
            }
        }
    }

    Ok(saved_count)
}

async fn import_database(
    db: Arc<NostrLMDB>,
    input_dir: PathBuf,
    skip_confirmation: bool,
    skip_errors: bool,
) -> Result<()> {
    if !input_dir.exists() {
        return Err(anyhow::anyhow!(
            "Input directory does not exist: {:?}",
            input_dir
        ));
    }

    // Find all JSONL files in the input directory
    let entries: Vec<_> = fs::read_dir(&input_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();

    if entries.is_empty() {
        return Err(anyhow::anyhow!(
            "No JSONL files found in input directory: {:?}",
            input_dir
        ));
    }

    info!("Found {} JSONL files to import", entries.len());

    // Show files to be imported and ask for confirmation
    if !skip_confirmation {
        println!("\nFiles to be imported:");
        println!("====================");
        for entry in &entries {
            if let Some(filename) = entry.file_name().to_str() {
                if let Some(scope) = parse_scope_from_filename(filename) {
                    println!("- {filename} -> {scope:?}");
                }
            }
        }

        print!("\nDo you want to proceed with import? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            info!("Import cancelled by user.");
            return Ok(());
        }
    }

    let progress_bar = ProgressBar::new(entries.len() as u64);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} files ({eta})")?
            .progress_chars("#>-"),
    );

    let mut total_imported = 0u64;
    let mut total_errors = 0u64;
    let mut processed_files = 0u64;

    for entry in entries {
        let path = entry.path();
        if let Some(filename) = path.file_name().and_then(|s| s.to_str()) {
            if let Some(scope) = parse_scope_from_filename(filename) {
                info!("Importing {} into scope {:?}", filename, scope);
                match import_scope(&db, &scope, &path, skip_errors).await {
                    Ok((imported, errors)) => {
                        total_imported += imported;
                        total_errors += errors;
                        processed_files += 1;
                        info!(
                            "Imported {} events from {} ({} errors)",
                            imported, filename, errors
                        );
                    }
                    Err(e) => {
                        error!("Failed to import {}: {}", filename, e);
                        if !skip_errors {
                            return Err(e);
                        }
                    }
                }
            } else {
                warn!("Skipping file with invalid naming pattern: {}", filename);
            }
        }
        progress_bar.inc(1);
    }

    progress_bar.finish_with_message("Import complete");

    info!(
        "Successfully imported {} events from {} files ({} errors)",
        total_imported, processed_files, total_errors
    );

    if total_errors > 0 && skip_errors {
        warn!(
            "{} errors occurred during import but were skipped",
            total_errors
        );
    }

    info!("\x1b[1;33mIMPORTANT: You must restart the relay server for these changes to take effect!\x1b[0m");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_tracing();
    let args = Args::parse();

    match args.command {
        Commands::Export {
            db,
            output,
            force,
            include_count,
        } => {
            info!("Opening database at {:?}", db);
            let database = Arc::new(
                NostrLMDB::open(&db)
                    .with_context(|| format!("Failed to open database at {db:?}"))?,
            );

            export_database(database, output, force, include_count).await
        }
        Commands::Import {
            db,
            input,
            yes,
            skip_errors,
        } => {
            info!("Opening database at {:?}", db);
            let database = Arc::new(
                NostrLMDB::open(&db)
                    .with_context(|| format!("Failed to open database at {db:?}"))?,
            );

            import_database(database, input, yes, skip_errors).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scope_to_filename() {
        assert_eq!(
            scope_to_filename(&Scope::Default, false, None),
            "scope_default.jsonl"
        );
        assert_eq!(
            scope_to_filename(&Scope::Default, true, Some(100)),
            "scope_default_100.jsonl"
        );
        assert_eq!(
            scope_to_filename(&Scope::named("test").unwrap(), false, None),
            "scope_test.jsonl"
        );
        assert_eq!(
            scope_to_filename(&Scope::named("test-scope").unwrap(), true, Some(50)),
            "scope_test-scope_50.jsonl"
        );
    }

    #[test]
    fn test_parse_scope_from_filename() {
        assert_eq!(
            parse_scope_from_filename("scope_default.jsonl"),
            Some(Scope::Default)
        );
        assert_eq!(
            parse_scope_from_filename("scope_default_100.jsonl"),
            Some(Scope::Default)
        );
        assert_eq!(
            parse_scope_from_filename("scope_test.jsonl"),
            Some(Scope::named("test").unwrap())
        );
        assert_eq!(
            parse_scope_from_filename("scope_test_scope.jsonl"),
            Some(Scope::named("test_scope").unwrap())
        );
        assert_eq!(
            parse_scope_from_filename("scope_test_scope_500.jsonl"),
            Some(Scope::named("test_scope").unwrap())
        );
        assert_eq!(parse_scope_from_filename("invalid.txt"), None);
        assert_eq!(parse_scope_from_filename("notscope_test.jsonl"), None);
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("test"), "test");
        assert_eq!(sanitize_filename("test-scope"), "test-scope");
        assert_eq!(sanitize_filename("test_scope"), "test_scope");
        assert_eq!(sanitize_filename("test/scope"), "test_scope");
        assert_eq!(sanitize_filename("test@scope!"), "test_scope_");
    }
}
