//! Static dataset export for the web viewer.
//!
//! Produces a fully self-contained `web/data/` directory — no server
//! needed, the SPA gunzips everything client-side:
//!
//!   catalog.json.gz     one compact record per lab (fetched or not)
//!   detail/{xx}.json.gz full detail per fetched adventure, grouped
//!                       into ≤256 shards by the guid's first byte
//!
//! Re-run after fetch/reviews finish to refresh the dataset.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Map, Value};
use tracing::info;

use crate::db::Db;

pub struct Args {
    /// Output directory (created if missing).
    pub out: PathBuf,
}

fn gz_write(path: &PathBuf, v: &Value) -> Result<u64> {
    let raw = serde_json::to_vec(v)?;
    let mut e = GzEncoder::new(Vec::new(), Compression::fast());
    e.write_all(&raw)?;
    let gz = e.finish()?;
    fs::write(path, &gz).with_context(|| format!("write {}", path.display()))?;
    Ok(gz.len() as u64)
}

/// Short-key catalog record. `None` fields are omitted to save bytes.
fn catalog_entry(
    g: &str,
    v: &Value,
    fetched: bool,
    owner: Option<&str>,
    vc: i64,
    ac: i64,
) -> Value {
    let (la, lo) = (
        v.pointer("/location/latitude").and_then(|x| x.as_f64()),
        v.pointer("/location/longitude").and_then(|x| x.as_f64()),
    );
    let mut m = Map::new();
    m.insert("g".into(), json!(g));
    if let Some(t) = v.get("title").and_then(|x| x.as_str()) {
        m.insert("t".into(), json!(t));
    }
    if let (Some(a), Some(b)) = (la, lo) {
        m.insert("la".into(), json!(a));
        m.insert("lo".into(), json!(b));
    }
    if let Some(t) = v.get("adventureType").and_then(|x| x.as_str()) {
        m.insert("ty".into(), json!(t));
    }
    for (src, dst) in [
        ("ratingsAverage", "ra"),
        ("ratingsTotalCount", "rc"),
        ("stagesTotalCount", "sc"),
        ("completionCount", "cc"),
        ("publishedUtc", "p"),
        ("keyImageUrl", "img"),
        ("visibility", "vis"),
        ("medianTimeToComplete", "mt"),
    ] {
        if let Some(x) = v.get(src).filter(|x| !x.is_null()) {
            m.insert(dst.into(), x.clone());
        }
    }
    if let Some(o) = owner {
        m.insert("o".into(), json!(o));
    }
    if vc > 0 {
        m.insert("vc".into(), json!(vc));
    }
    if ac > 0 {
        m.insert("ac".into(), json!(ac));
    }
    if v.get("isHighlyRecommended").and_then(|x| x.as_bool()) == Some(true) {
        m.insert("hr".into(), json!(true));
    }
    if v.get("isArchived").and_then(|x| x.as_bool()) == Some(true) {
        m.insert("arch".into(), json!(true));
    }
    if v.get("isTest").and_then(|x| x.as_bool()) == Some(true) {
        m.insert("test".into(), json!(true));
    }
    if let Some(th) = v.get("adventureThemes") {
        m.insert("th".into(), th.clone());
    }
    m.insert("f".into(), json!(fetched));
    Value::Object(m)
}

/// Full detail record for a fetched adventure: summary fields +
/// stages + reviews straight from raw JSON (full fidelity), plus
/// cracked answers keyed by stage index.
fn detail_entry(
    v: &Value,
    reviews: Vec<Value>,
    answers: &[(i64, String, Option<String>, String)],
) -> Value {
    let mut m = Map::new();
    for k in [
        "adventureGuid", "title", "description", "adventureType", "location",
        "medianTimeToComplete", "ratingsAverage", "ratingsTotalCount",
        "reviewsTotalCount", "completionCount", "recommendedCount",
        "journalsTotalCount", "ownerUsername", "ownerPublicGuid",
        "publishedUtc", "createdUtc", "visibility", "customAccessCode",
        "adventureThemes", "keyImageUrl", "isArchived", "isTest",
        "isHighlyRecommended", "stageSummaries",
    ] {
        if let Some(x) = v.get(k) {
            m.insert(k.into(), x.clone());
        }
    }
    if !reviews.is_empty() {
        m.insert("reviews".into(), Value::Array(reviews));
    }
    if !answers.is_empty() {
        // stage_index -> [{a: plaintext, d?: display, m: method}]
        let mut ans = Map::new();
        for (idx, p, d, method) in answers {
            let mut e = Map::new();
            e.insert("a".into(), json!(p));
            if let Some(d) = d {
                e.insert("d".into(), json!(d));
            }
            e.insert("m".into(), json!(method));
            ans.entry(idx.to_string())
                .or_insert_with(|| Value::Array(vec![]))
                .as_array_mut()
                .unwrap()
                .push(Value::Object(e));
        }
        m.insert("answers".into(), Value::Object(ans));
    }
    Value::Object(m)
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    fs::create_dir_all(args.out.join("detail"))?;

    // One 2-hex-guid shard at a time: query rows, reviews and cracks for
    // that prefix only, write the shard, drop everything. Peak memory is
    // ~1/256 of the dataset — the whole-table approach OOMed the box.
    let mut catalog = Vec::new();
    let mut shard_bytes = 0u64;
    let mut total = 0usize;
    let mut n_shards = 0usize;
    for i in 0u32..256 {
        let prefix = format!("{i:02x}");
        let rows = db.catalog_rows_prefix(prefix.clone()).await?;
        if rows.is_empty() {
            continue;
        }
        let cracks = db.cracks_prefix(prefix.clone()).await?;
        let reviews = db.reviews_prefix(prefix.clone(), 20).await?;
        let mut shard = Map::new();
        for r in &rows {
            if let Some(adv) = &r.adv_json {
                let v: Value = serde_json::from_str(adv)?;
                let ans = cracks.get(&r.guid).cloned().unwrap_or_default();
                catalog.push(catalog_entry(
                    &r.guid,
                    &v,
                    true,
                    r.owner_username.as_deref(),
                    r.reviews_total_count.unwrap_or(0),
                    ans.len() as i64,
                ));
                let revs = reviews.get(&r.guid).cloned().unwrap_or_default();
                shard.insert(r.guid.clone(), detail_entry(&v, revs, &ans));
                total += 1;
            } else if let Some(lab) = &r.lab_json {
                let v: Value = serde_json::from_str(lab)?;
                catalog.push(catalog_entry(&r.guid, &v, false, None, 0, 0));
            }
        }
        let path = args.out.join("detail").join(format!("{prefix}.json.gz"));
        shard_bytes += gz_write(&path, &Value::Object(shard))?;
        n_shards += 1;
        if n_shards.is_multiple_of(32) {
            info!("detail: {} shards written", n_shards);
        }
    }

    let n_cat = catalog.len();
    let cat_bytes = gz_write(&args.out.join("catalog.json.gz"), &Value::Array(catalog))?;
    info!(
        "catalog: {} entries -> catalog.json.gz ({:.1} MB)",
        n_cat,
        cat_bytes as f64 / 1e6
    );
    info!(
        "detail: {} adventures in {} shards ({:.1} MB gz)",
        total,
        n_shards,
        shard_bytes as f64 / 1e6
    );
    info!("export complete -> {}", args.out.display());
    Ok(())
}
