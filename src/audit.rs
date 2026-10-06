//! Occasional deep-truth coverage audit for one split cell: capture the
//! parent (cliff-capped at the 10000 window) and its 4 children (exact
//! when each is at most 10000), then check every parent-only guid against
//! the local discovery set. Anything missing locally is churn or a genuine
//! coverage hole — the counts tell which.
//!
//! Exit status is always success; this is an informational audit, and a
//! handful of missing labs is expected churn (newly published). Worry
//! when `missing` is a substantial share of `holes`.

use std::collections::HashSet;

use anyhow::Result;
use tracing::{info, warn};

use crate::api::{ApiError, Client, TAKE};
use crate::db::Db;
use crate::geo::{self, Cell};

pub struct Args {
    pub lat: f64,
    pub lon: f64,
    pub radius: f64,
}

/// One cell's full item-id set, stopping gracefully at the 10000 window
/// cliff (HTTP 500 past it). Returns (reported total, captured ids).
async fn fetch_ids(
    client: &Client,
    cell: &Cell,
    label: &str,
) -> Result<(u64, HashSet<String>)> {
    let first = client.search(cell, TAKE, 0).await?;
    let total = first.total_count;
    let mut ids = HashSet::new();
    for it in &first.items {
        if let Some(g) = it.get("adventureGuid").and_then(|g| g.as_str()) {
            ids.insert(g.to_string());
        }
    }
    let mut skip = TAKE;
    loop {
        match client.search(cell, TAKE, skip).await {
            Ok(page) => {
                if page.items.is_empty() {
                    break;
                }
                for it in &page.items {
                    if let Some(g) = it.get("adventureGuid").and_then(|g| g.as_str()) {
                        ids.insert(g.to_string());
                    }
                }
                if page.items.len() < TAKE {
                    break;
                }
                skip += TAKE;
            }
            Err(ApiError::Status(code, _)) if skip >= 9500 => {
                info!("{label}: stopped at skip={skip} (window cliff, http {code})");
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    info!("{label}: total={total} captured={}", ids.len());
    Ok((total, ids))
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    let client = Client::new(10.0, None)?;
    let parent = Cell::new(args.lat, args.lon, args.radius);
    info!(
        "audit: parent {} lat={} lon={} radius={}m",
        parent.id, parent.lat, parent.lon, parent.radius
    );
    let (ptotal, pids) = fetch_ids(&client, &parent, "parent").await?;
    let mut union = HashSet::new();
    let mut exact = true;
    for (i, child) in geo::subdivide(&parent).iter().enumerate() {
        let (ctotal, cids) = fetch_ids(&client, child, &format!("child{i}")).await?;
        if ctotal > 10000 {
            warn!("child{i} total={ctotal} exceeds window — its set is cliff-capped");
            exact = false;
        }
        union.extend(cids);
    }
    let holes: Vec<String> = pids.difference(&union).cloned().collect();
    let mut missing = Vec::new();
    for g in &holes {
        if !db.has_lab(g).await? {
            missing.push(g.clone());
        }
    }
    info!(
        "audit: parent_total={ptotal} parent_items={} children_union={} \
         holes={} in_db={} missing={} children_exact={exact}",
        pids.len(),
        union.len(),
        holes.len(),
        holes.len() - missing.len(),
        missing.len(),
    );
    for g in missing.iter().take(50) {
        warn!("audit: missing locally: {g}");
    }
    if missing.len() > 50 {
        warn!("audit: ... and {} more", missing.len() - 50);
    }
    Ok(())
}
