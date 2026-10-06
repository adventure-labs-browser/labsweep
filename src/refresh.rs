//! Daily refresh pass: re-queue crawl cells, pick up new labs,
//! re-fetch stalest details, re-scrape stalest reviews.
//!
//! Never deletes: updates, removals and restores all append to the
//! `*_versions` history tables (see db.rs). Every stage is resumable,
//! so a killed run continues where it left off on the next pass.

use anyhow::Result;
use tracing::info;

use crate::{crack, crawl, db::Db, fetch, reviews};

pub struct Args {
    pub crawl_rate: f64,
    pub fetch_rate: f64,
    pub reviews_rate: f64,
    pub fetch_max: usize,
    pub reviews_max: usize,
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    // Flow metric: labs discovered by THIS run = the measured miss count.
    // (The stock metric — est_missing — comes from verify at the end.)
    let labs_before = db.labs_count().await?;
    let n = db.requeue_done().await?;
    info!("refresh: {n} crawl cells re-queued");
    crawl::run(
        db.clone(),
        crawl::Args {
            concurrency: 16,
            rate: args.crawl_rate,
            seeds: 16,
            seed_radius_m: 12_000_000.0,
            min_radius_m: 1000.0,
            max_cells: 0,
            reset: false,
            reset_failed: false,
        },
    )
    .await?;
    info!("refresh: crawl done — fetching new details");
    fetch::run(
        db.clone(),
        fetch::Args {
            concurrency: 32,
            rate: args.fetch_rate,
            bearer: String::new(),
            max: 0,
            restudy: false,
        },
    )
    .await?;
    info!(
        "refresh: new details done — restudying stalest (max {})",
        args.fetch_max
    );
    fetch::run(
        db.clone(),
        fetch::Args {
            concurrency: 32,
            rate: args.fetch_rate,
            bearer: String::new(),
            max: args.fetch_max,
            restudy: true,
        },
    )
    .await?;
    info!("refresh: restudy done — scraping new reviews");
    reviews::run(
        db.clone(),
        reviews::Args {
            concurrency: 64,
            rate: args.reviews_rate,
            bearer: String::new(),
            max: 0,
            revisit: false,
        },
    )
    .await?;
    info!(
        "refresh: new reviews done — revisiting stalest (max {})",
        args.reviews_max
    );
    reviews::run(
        db.clone(),
        reviews::Args {
            concurrency: 64,
            rate: args.reviews_rate,
            bearer: String::new(),
            max: args.reviews_max,
            revisit: true,
        },
    )
    .await?;
    // Cheap local crack phases over newly fetched hashes (no wordlist,
    // no brute force — those stay manual). Keeps the cracks table moving
    // with the scrape instead of frozen.
    info!("refresh: cracking new hashes (cheap phases)");
    crack::run(
        db.clone(),
        crack::Args {
            phases: "multichoice,numeric,patterns".to_string(),
            wordlist: None,
            brute_len: 0,
            export_hashes: None,
            import_hashes: None,
            salt: None,
        },
    )
    .await?;
    info!(
        "refresh: done — {}",
        db.stats().await?
    );
    let labs_after = db.labs_count().await?;
    info!(
        "refresh: +{} new labs this run ({labs_before} -> {labs_after})",
        labs_after.saturating_sub(labs_before),
    );
    Ok(())
}
