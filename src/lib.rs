//! Comifuro catalog scraper: map discovery, vision extraction, SQLite, and JSON.
mod batch;
mod catalog;
mod d1;
mod dedupe;
mod instagram;
mod map;
mod net;
mod quota;
mod social;
mod storage;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use std::{path::PathBuf, time::Duration};

const MODEL: &str = "nvidia/nemotron-3.5-super-vl:free";

#[derive(Parser)]
#[command(
    version,
    about = "Extract Comifuro catalog merchandise using OpenRouter vision"
)]
struct Cli {
    #[arg(long, default_value = "data/comi.sqlite")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Detect duplicate images/items in already scraped data without using the VLM.
    Dedupe {
        #[arg(long)]
        event: String,
    },
    /// Process every remaining catalog batch, checkpointing and optionally waiting for quota.
    ScrapeAll {
        #[arg(long)]
        event: String,
        #[arg(long,default_value="25",value_parser=positive)]
        batch_size: usize,
        #[arg(long,default_value="20",value_parser=positive)]
        max_images: usize,
        #[arg(long,default_value="4",value_parser=interval)]
        interval: f64,
        #[arg(long)]
        upload: bool,
        #[arg(long)]
        wait_for_quota: bool,
    },
    /// Scrape one public catalog post per circle, with resumable attempt tracking.
    Scrape {
        #[arg(long)]
        event: String,
        #[arg(long,default_value="25",value_parser=positive)]
        limit: usize,
        #[arg(long,default_value="20",value_parser=positive)]
        max_images: usize,
        #[arg(long,default_value="4",value_parser=interval)]
        interval: f64,
        /// Upload catalog results to an already initialized D1 database every five sources.
        #[arg(long)]
        upload: bool,
        #[arg(long)]
        retry_failed: bool,
    },
    /// Check D1 credentials using a read-only query.
    D1Check,
    /// Create missing D1 tables and upload local rows with idempotent upserts.
    D1Push {
        /// Upload extraction/item tables only; booth metadata must already exist in D1.
        #[arg(long)]
        catalogs_only: bool,
        /// Re-upload rows even if the local upload ledger says they are unchanged.
        #[arg(long)]
        force: bool,
    },
    /// Import circle metadata and catalog links from the CF map.
    Sync {
        #[arg(long, default_value = "https://cf.mikira.id/")]
        map_url: String,
        /// Event identifier, such as cf23 or cf24. Required to avoid mixing editions.
        #[arg(long)]
        event: String,
        #[arg(long)]
        event_name: Option<String>,
    },
    /// Link a participation to a verified existing circle identity across events.
    LinkCircle {
        #[arg(long)]
        participation: String,
        #[arg(long)]
        profile: String,
    },
    /// Find circles and their exact identifiers.
    List {
        #[arg(default_value = "")]
        query: String,
    },
    /// Extract a local image, public image URL, or public HTML catalog page.
    Ingest {
        source: String,
        #[command(flatten)]
        options: ExtractionOptions,
    },
    /// Process catalog links imported from the map for one circle.
    Crawl {
        #[command(flatten)]
        options: ExtractionOptions,
        #[arg(long, default_value = "1", value_parser = positive)]
        limit: usize,
    },
    /// Export JSON for a future Comi catalog integration.
    Export {
        #[arg(long, default_value = "data/catalogs.json")]
        output: PathBuf,
    },
}

#[derive(Args)]
struct ExtractionOptions {
    #[arg(long)]
    circle: String,
    #[arg(long, default_value = "20", value_parser = positive)]
    max_images: usize,
    #[arg(long, default_value = "4", value_parser = interval)]
    interval: f64,
}

fn positive(raw: &str) -> std::result::Result<usize, String> {
    raw.parse::<usize>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

fn interval(raw: &str) -> std::result::Result<f64, String> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && (0.0..=3600.0).contains(v))
        .ok_or_else(|| "must be a finite number between 0 and 3600".into())
}

/// Run the command line interface, loading an optional `.env` without overriding environment variables.
///
/// # Errors
/// Returns an error on invalid configuration, source failures, or persistence errors.
pub fn run() -> Result<()> {
    if let Err(error) = dotenvy::dotenv() {
        ensure!(error.not_found(), "could not load .env");
    }
    let cli = Cli::parse();
    let mut db = storage::open(&cli.db)?;
    match cli.command {
        Command::Dedupe { event } => {
            dedupe::backfill(&db, &event)?;
            println!("Duplicate detection refreshed for {event}");
        }
        Command::ScrapeAll {
            event,
            batch_size,
            max_images,
            interval,
            upload,
            wait_for_quota,
        } => batch::all(
            &db,
            &event,
            batch_size,
            max_images,
            interval,
            upload,
            wait_for_quota,
        )?,
        Command::Scrape {
            event,
            limit,
            max_images,
            interval,
            upload,
            retry_failed,
        } => batch::run(
            &db,
            &event,
            limit,
            max_images,
            interval,
            upload,
            retry_failed,
        )?,
        Command::D1Check => {
            d1::D1::from_env()?.check()?;
            println!("D1 connection verified");
        }
        Command::D1Push {
            catalogs_only,
            force,
        } => {
            let d1 = d1::D1::from_env()?;
            if force {
                d1.reset_upload_state(&db)?;
            }
            d1.push(&db, catalogs_only)?;
        }
        Command::Sync {
            map_url,
            event,
            event_name,
        } => {
            let http = net::Http::new()?;
            let circles = map::load(&http, &map_url)?;
            let name = event_name.unwrap_or_else(|| event.to_uppercase());
            storage::sync(&mut db, &circles, &event, &name)?;
            println!(
                "Synced {} circles and {} catalog links",
                circles.len(),
                circles.iter().map(|c| c.sources.len()).sum::<usize>()
            );
        }
        Command::LinkCircle {
            participation,
            profile,
        } => {
            storage::link_profile(&db, &participation, &profile)?;
            println!("Linked event participation to circle profile");
        }
        Command::List { query } => {
            for (id, name) in storage::list(&db, &query)? {
                println!("{id}\t{name}");
            }
        }
        Command::Export { output } => storage::export(&db, &output)?,
        Command::Ingest { source, options } => ingest(&db, &[source], &options)?,
        Command::Crawl { options, limit } => {
            let sources = storage::sources(&db, &options.circle, limit)?;
            ensure!(
                !sources.is_empty(),
                "no catalog links found; use ingest with a downloaded image"
            );
            ingest(&db, &sources, &options)?;
        }
    }
    Ok(())
}

fn ingest(
    db: &rusqlite::Connection,
    sources: &[String],
    options: &ExtractionOptions,
) -> Result<()> {
    ensure!(
        storage::has_circle(db, &options.circle)?,
        "unknown circle id; run sync/list first"
    );
    let key = std::env::var("OPENROUTER_API_KEY")
        .context("set OPENROUTER_API_KEY in your environment or .env")?;
    ensure!(!key.trim().is_empty(), "OPENROUTER_API_KEY is empty");
    let model = std::env::var("OPENROUTER_MODEL").unwrap_or_else(|_| MODEL.into());
    ensure!(model.ends_with(":free"), "only :free models are allowed");
    let http = net::Http::new()?;
    let vlm = catalog::Vlm::new(
        &http,
        &model,
        &key,
        Duration::from_secs_f64(options.interval),
    );
    let mut failed = 0;
    let mut failures = Vec::new();
    for source in sources {
        if let Err(error) =
            catalog::process(db, &http, &vlm, &options.circle, source, options.max_images)
        {
            // Source URLs and upstream response bodies are intentionally excluded from diagnostics.
            eprintln!("source failed: {error:#}");
            if error.downcast_ref::<quota::Stop>().is_some() {
                return Err(error);
            }
            failures.push(format!("{error:#}"));
            failed += 1;
        }
    }
    if failed > 0 {
        bail!(
            "{failed} source(s) failed; successful pages remain saved: {}",
            failures.join("; ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_limits() {
        for raw in ["NaN", "inf", "-1", "3601"] {
            assert!(interval(raw).is_err());
        }
        assert!(positive("0").is_err());
        assert_eq!(positive("8").unwrap(), 8);
    }
}
