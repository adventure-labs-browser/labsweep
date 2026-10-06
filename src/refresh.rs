//! Daily refresh pass: re-queue crawl cells, pick up new labs,
//! re-fetch stalest details, re-scrape stalest reviews.
//!
//! Never deletes: updates, removals and restores all append to the
//! `*_versions` history tables (see db.rs). Every stage is resumable,
//! so a killed run continues where it left off on the next pass.

use anyhow::Result;
use tracing::info;

use crate::{crawl, db::Db, fetch, reviews};

pub struct Args {
    pub crawl_rate: f64,
    pub fetch_rate: f64,
    pub reviews_rate: f64,
    pub fetch_max: usize,
    pub reviews_max: usize,
}

pub async fn run(db: Db, args: Args) -> Result<()> {
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
    info!("refresh: done — {}", db.stats().await?);
    Ok(())
}
