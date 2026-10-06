//! Stage 2 — fetch the full detail record for every discovered GUID.
//!
//! With auth (stored creds or `--bearer`), detail responses include
//! `findCodeHashBase16v2` / `answerCodeHashesBase16v2` — the md5 answer
//! hashes (semantics + normalization documented on the `stages` table
//! in db.rs). Rows fetched unauthenticated lack them and are re-queued
//! automatically when a bearer is present.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures::StreamExt;
use tracing::{info, warn};

use crate::api::{base_headers, Client};
use crate::auth::Auth;
use crate::db::Db;

const PROGRESS_EVERY: u64 = 1000;

pub struct Args {
    pub concurrency: usize,
    pub rate: f64,
    /// Manual token override; stored credentials (labsweep auth login)
    /// are used automatically when this is empty.
    pub bearer: String,
    /// Fetch at most N GUIDs (0 = all pending).
    pub max: usize,
    /// Refresh mode: re-fetch already-fetched details stalest-first
    /// instead of only pending GUIDs. Updates version history.
    pub restudy: bool,
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    let auth = Auth::load(&db, base_headers()).await?;
    let client = if !args.bearer.is_empty() {
        Arc::new(Client::new(args.rate, Some(args.bearer))?)
    } else if let Some(a) = auth {
        info!(
            "authenticated as {} (auto-refreshing)",
            a.username().await
        );
        Arc::new(Client::authed(args.rate, Arc::new(a))?)
    } else {
        Arc::new(Client::new(args.rate, None)?)
    };
    let has_bearer = client.has_bearer();
    let guids = if args.restudy {
        let lim = if args.max > 0 { args.max as i64 } else { -1 };
        let g = db.refetch_guids(lim).await?;
        info!("restudy: {} adventures to re-fetch (stalest first)", g.len());
        g
    } else {
        db.pending_guids(has_bearer).await?
    };
    let total = guids.len();
    if total == 0 {
        info!("nothing to fetch — {}", db.stats().await?);
        return Ok(());
    }
    if has_bearer {
        info!("bearer set: rows missing answer hashes will be re-fetched");
    }
    info!("{total} adventures to fetch");
    let stop = crate::util::shutdown_flag();
    let done = Arc::new(AtomicU64::new(0));
    let warned_no_hash = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();

    let stream = futures::stream::iter(guids.into_iter().enumerate());
    stream
        .for_each_concurrent(args.concurrency, |(idx, guid)| {
            let (db, client, stop, done, warned) = (
                db.clone(),
                client.clone(),
                stop.clone(),
                done.clone(),
                warned_no_hash.clone(),
            );
            async move {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                if args.max > 0 && idx >= args.max {
                    return;
                }
                let d = client.detail(&guid).await;
                if client.has_bearer() && !warned.load(Ordering::SeqCst) {
                    if let Some(v) = &d.json {
                        let stages = v.get("stageSummaries").and_then(|s| s.as_array());
                        let has_hashes = stages.is_some_and(|ss| {
                            ss.iter().any(|s| s.get("findCodeHashBase16v2").is_some())
                        });
                        if stages.is_some_and(|ss| !ss.is_empty())
                            && !has_hashes
                            && warned
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                        {
                            warn!("{guid}: no findCodeHashBase16v2 — bearer token may be expired");
                        }
                    }
                }
                if let Err(e) = db
                    .save_adventure(guid.clone(), d.json, d.status, d.error)
                    .await
                {
                    warn!("{guid}: db save failed: {e}");
                }
                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                if n % PROGRESS_EVERY == 0 {
                    let rate = n as f64 / t0.elapsed().as_secs_f64();
                    let eta = (total as u64).saturating_sub(n) as f64 / rate.max(0.01);
                    info!("{n}/{total} fetched ({rate:.1}/s, eta {}m)", (eta / 60.0) as u64);
                }
            }
        })
        .await;

    info!("done: {}", db.stats().await?);
    Ok(())
}
