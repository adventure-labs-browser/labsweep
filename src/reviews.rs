//! Stage 3 — fetch every review for every adventure that has them.
//!
//! Endpoint (reversed from the Android app):
//!   POST /v1/public/adventures/{guid}/reviews/search?skip=N&take=N
//! No auth needed for reading, but the authed client is used when
//! credentials are stored — same client, same rate limiter.
//!
//! Resumable at adventure granularity: `adventures.reviews_done`
//! flips only after all of an adventure's pages are persisted in one
//! transaction, so a kill mid-adventure just re-fetches that one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures::StreamExt;
use tracing::{info, warn};

use crate::api::{base_headers, Client};
use crate::auth::Auth;
use crate::db::Db;

const TAKE: usize = 500;
const PROGRESS_EVERY: u64 = 1000;

pub struct Args {
    pub concurrency: usize,
    pub rate: f64,
    /// Manual token override; stored credentials used automatically.
    pub bearer: String,
    /// Process at most N adventures (0 = all pending).
    pub max: usize,
    /// Refresh mode: re-scrape already-scraped adventures stalest-first
    /// instead of only pending ones. Picks up new/edited/vanished reviews.
    pub revisit: bool,
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    let auth = Auth::load(&db, base_headers()).await?;
    let client = if !args.bearer.is_empty() {
        Arc::new(Client::new(args.rate, Some(args.bearer))?)
    } else if let Some(a) = auth {
        info!("authenticated as {} (auto-refreshing)", a.username().await);
        Arc::new(Client::authed(args.rate, Arc::new(a))?)
    } else {
        Arc::new(Client::new(args.rate, None)?)
    };

    let guids = if args.revisit {
        let lim = if args.max > 0 { args.max as i64 } else { -1 };
        let g = db.revisit_review_guids(lim).await?;
        info!("revisit: {total} adventures to re-scrape (stalest first)", total = g.len());
        g
    } else {
        db.pending_review_guids().await?
    };
    let total = guids.len();
    if total == 0 {
        info!("no adventures pending reviews — {}", db.stats().await?);
        return Ok(());
    }
    info!("{total} adventures with reviews to scrape");

    let stop = crate::util::shutdown_flag();
    let done = Arc::new(AtomicU64::new(0));
    let review_count = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();

    let stream = futures::stream::iter(guids.into_iter().enumerate());
    stream
        .for_each_concurrent(args.concurrency, |(idx, guid)| {
            let (db, client, stop, done, review_count) = (
                db.clone(),
                client.clone(),
                stop.clone(),
                done.clone(),
                review_count.clone(),
            );
            async move {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                if args.max > 0 && idx >= args.max {
                    return;
                }
                // Page 0 first: it tells totalCount and seeds the revisit
                // short-circuit below.
                let first = client.reviews(&guid, 0, TAKE).await;
                if let Some(e) = first.error {
                    warn!("{guid}: reviews failed: {e}");
                    let _ = db.mark_reviews_failed(guid.clone(), e).await;
                    let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                    if n % PROGRESS_EVERY == 0 {
                        progress(n, total, &review_count, &t0);
                    }
                    return;
                }
                let mut items = first.items;
                let mut review_total = first.total_count;
                // Revisit short-circuit: same total AND same newest id as
                // stored means nothing arrived, left, or (almost surely)
                // changed — one request instead of full pagination. New,
                // removed, or swapped reviews change one of the two and
                // fall through to the full scrape. (Silent in-place text
                // edits slip through; accepted — deletions/additions, the
                // cases that matter for history, never do.)
                if args.revisit {
                    if let Ok((stored_total, stored_max)) =
                        db.review_state(guid.clone()).await
                    {
                        let live_max = items
                            .iter()
                            .filter_map(|v| {
                                v.get("id").and_then(|x| {
                                    x.as_i64().or_else(|| {
                                        x.as_u64().map(|n| n as i64)
                                    })
                                })
                            })
                            .max();
                        if review_total as i64 == stored_total
                            && live_max == stored_max
                        {
                            if db.mark_reviews_checked(guid.clone()).await.is_err() {
                                warn!("{guid}: db check-in failed");
                            }
                            let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                            if n % PROGRESS_EVERY == 0 {
                                progress(n, total, &review_count, &t0);
                            }
                            return;
                        }
                    }
                }
                // Paginate the rest; the rare adventure with >TAKE reviews
                // gets follow-up pages.
                let mut skip = TAKE;
                let mut failed: Option<String> = None;
                while (items.len() as u64) < review_total && review_total != 0 {
                    let r = client.reviews(&guid, skip, TAKE).await;
                    if let Some(e) = r.error {
                        failed = Some(e);
                        break;
                    }
                    review_total = r.total_count;
                    items.extend(r.items);
                    skip += TAKE;
                }
                match failed {
                    Some(e) => {
                        warn!("{guid}: reviews failed: {e}");
                        let _ = db.mark_reviews_failed(guid.clone(), e).await;
                    }
                    None => {
                        let count = items.len() as u64;
                        if let Err(e) = db
                            .save_reviews(guid.clone(), review_total as i64, items)
                            .await
                        {
                            warn!("{guid}: db save failed: {e}");
                        } else {
                            review_count.fetch_add(count, Ordering::SeqCst);
                        }
                    }
                }
                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                if n % PROGRESS_EVERY == 0 {
                    let rate = n as f64 / t0.elapsed().as_secs_f64();
                    let eta = (total as u64).saturating_sub(n) as f64 / rate.max(0.01);
                    info!(
                        "{n}/{total} adventures scraped, {} reviews ({rate:.1}/s, eta {}m)",
                        review_count.load(Ordering::SeqCst),
                        (eta / 60.0) as u64
                    );
                }
            }
        })
        .await;

    info!(
        "done: {} reviews collected | {}",
        review_count.load(Ordering::SeqCst),
        db.stats().await?
    );
    Ok(())
}

fn progress(n: u64, total: usize, review_count: &Arc<AtomicU64>, t0: &Instant) {
    let rate = n as f64 / t0.elapsed().as_secs_f64();
    let eta = (total as u64).saturating_sub(n) as f64 / rate.max(0.01);
    info!(
        "{n}/{total} adventures scraped, {} reviews ({rate:.1}/s, eta {}m)",
        review_count.load(Ordering::SeqCst),
        (eta / 60.0) as u64
    );
}
