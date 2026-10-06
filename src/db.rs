//! Single-database persistence. One `Mutex<Connection>` serializes all
//! SQLite access — the old version had N thread-local connections
//! competing for the same writer lock, which SQLite serializes anyway.
//!
//! Resumability contract:
//!   - `queue.next_skip` records how far a cell has paginated. Every page
//!     writes its items AND advances next_skip in ONE transaction, so a
//!     crash mid-cell resumes at the exact page offset.
//!   - `in_progress` cells older than the reap threshold are reset to
//!     `pending` on startup and continue from next_skip.
//!   - `labs.guid` dedup makes re-inserting already-seen items a no-op.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use tokio::task::spawn_blocking;

use crate::geo::Cell;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS queue (
    id TEXT PRIMARY KEY,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    radius REAL NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending|in_progress|done|subdivided|failed
    attempts INTEGER NOT NULL DEFAULT 0,
    total_count INTEGER,
    next_skip INTEGER NOT NULL DEFAULT 0,    -- pagination checkpoint for resume
    inserted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS queue_status_idx ON queue(status);

CREATE TABLE IF NOT EXISTS labs (
    guid TEXT PRIMARY KEY,
    raw_json TEXT NOT NULL,
    source TEXT,
    inserted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS adventures (
    guid TEXT PRIMARY KEY,
    title TEXT,
    description TEXT,
    adventure_type TEXT,
    median_time_to_complete INTEGER,
    ratings_average REAL,
    ratings_total_count INTEGER,
    reviews_total_count INTEGER,
    completion_count INTEGER,
    recommended_count INTEGER,
    journals_total_count INTEGER,
    completed_stages_count INTEGER,
    stages_total_count INTEGER,
    owner_username TEXT,
    owner_public_guid TEXT,
    is_archived INTEGER,
    is_test INTEGER,
    is_highly_recommended INTEGER,
    visibility TEXT,
    published_utc TEXT,
    created_utc TEXT,
    location_lat REAL,
    location_lon REAL,
    custom_access_code TEXT,
    themes_json TEXT,
    raw_json TEXT NOT NULL,
    http_status INTEGER,
    error TEXT,
    fetched_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Never-delete history (refresh mode): removals are marks, updates
    -- append to adventure_versions. NULL status on old rows == 'active'.
    status TEXT NOT NULL DEFAULT 'active',   -- active|removed|unfetched
    content_hash TEXT,                        -- md5(raw_json); NULL = pre-history baseline
    removed_at TEXT,
    version_seq INTEGER NOT NULL DEFAULT 1,
    reviews_checked_at TEXT                   -- last full reviews re-scrape
);
CREATE INDEX IF NOT EXISTS idx_adv_pub ON adventures(published_utc);
CREATE INDEX IF NOT EXISTS idx_adv_status ON adventures(status);

-- stages.raw_json keeps the full API stage object, including the answer
-- hash fields (only present when fetched with a bearer token):
--
--   findCodeHashBase16V2      scalar — hash of the PRIMARY accepted answer.
--                             Always equals answerCodeHashesBase16V2[0]
--                             (verified against live data). Legacy field
--                             kept populated for single-answer stages.
--
--   answerCodeHashesBase16V2  list — hashes of ALL accepted answers
--                             (len 1-3 observed: alternates the creator
--                             registered; dupes can appear post-normalize).
--
-- Validation algorithm, byte-exact from the app's own client-side check
-- (com.groundspeak.react.adventures v1.71.0, Hermes bundle):
--
--   normalized = input
--       .replace(/\s/g,   '')   -- ALL whitespace removed
--       .replace(/‘|’/g,  "'")  -- curly → straight single quotes
--       .replace(/“|”/g,  '"')  -- curly → straight double quotes
--       .replace(/—|–/g,  '-')  -- em/en dashes → hyphen
--       .replace(/Σ/g,    'σ')  -- capital → lowercase sigma
--       .toLowerCase()
--   isValid  <=>  md5(playerPublicGuid + normalized) ∈ answerCodeHashes
--
-- The salt is the *fetching account's* publicGuid — hashes are
-- per-account, so the stored auth's public_guid is the crack salt.
-- challengeType == "Lodestone" stages skip validation entirely
-- (physical NFC/QR scan stages; rare — none observed in data so far).
-- "V2" names the current normalization scheme.
CREATE TABLE IF NOT EXISTS stages (
    adventure_guid TEXT NOT NULL,
    stage_index INTEGER NOT NULL,
    title TEXT,
    description TEXT,
    challenge_type TEXT,
    is_complete INTEGER,
    is_final INTEGER,
    geofencing_radius INTEGER,
    latitude REAL,
    longitude REAL,
    question TEXT,
    raw_json TEXT,
    status TEXT NOT NULL DEFAULT 'active',   -- active|removed
    content_hash TEXT,                        -- md5(raw_json); NULL = pre-history baseline
    removed_at TEXT,
    version_seq INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (adventure_guid, stage_index)
);

CREATE TABLE IF NOT EXISTS reviews (
    id INTEGER PRIMARY KEY,               -- API's numeric review id
    adventure_guid TEXT NOT NULL,
    rating INTEGER,
    review_text TEXT,
    player_username TEXT,
    player_public_guid TEXT,
    player_geocache_find_count INTEGER,
    player_completed_adventure_count INTEGER,
    recommended INTEGER,
    is_creator INTEGER,
    created_utc TEXT,
    completed_utc TEXT,
    images_json TEXT,
    tags_json TEXT,
    raw_json TEXT NOT NULL,
    fetched_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    status TEXT NOT NULL DEFAULT 'active',   -- active|removed
    content_hash TEXT,                        -- md5(raw_json); NULL = pre-history baseline
    removed_at TEXT,
    version_seq INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_reviews_adv ON reviews(adventure_guid);
CREATE INDEX IF NOT EXISTS idx_reviews_status ON reviews(status);

-- Never-delete version history. Each row is a snapshot of a live row as
-- it was when superseded (updated), tombstoned (removed/deleted) or
-- seen again after removal (restored). Parsed columns are re-derivable
-- from raw_json, so versions stay lean. Live tables always hold latest.
CREATE TABLE IF NOT EXISTS adventure_versions (
    adventure_guid TEXT NOT NULL,
    version_seq INTEGER NOT NULL,
    superseded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    change TEXT NOT NULL,              -- updated|removed|restored
    status TEXT,
    http_status INTEGER,
    raw_json TEXT,
    PRIMARY KEY (adventure_guid, version_seq)
);
CREATE TABLE IF NOT EXISTS stage_versions (
    adventure_guid TEXT NOT NULL,
    stage_index INTEGER NOT NULL,
    version_seq INTEGER NOT NULL,
    superseded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    change TEXT NOT NULL,              -- updated|deleted|removed|restored
    raw_json TEXT,
    PRIMARY KEY (adventure_guid, stage_index, version_seq)
);
CREATE TABLE IF NOT EXISTS review_versions (
    review_id INTEGER NOT NULL,
    version_seq INTEGER NOT NULL,
    superseded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    change TEXT NOT NULL,              -- updated|removed|restored
    raw_json TEXT,
    PRIMARY KEY (review_id, version_seq)
);

-- Cracked answers: one row per (stage, accepted-hash) we recovered.
-- plaintext is the normalized candidate that hashed; display keeps the
-- pre-normalization text when the source had it (multichoice option,
-- wordlist line). See the hashing spec on `stages` above — the salt is
-- meta.auth.public_guid and every hash shares it.
CREATE TABLE IF NOT EXISTS cracks (
    adventure_guid TEXT NOT NULL,
    stage_index INTEGER NOT NULL,
    hash TEXT NOT NULL,
    plaintext TEXT NOT NULL,
    display TEXT,
    method TEXT NOT NULL,               -- multichoice|numeric|corpus|wordlist|brute|hashcat
    cracked_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (adventure_guid, stage_index, hash)
);
CREATE INDEX IF NOT EXISTS idx_cracks_hash ON cracks(hash);

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
"#;

/// A claimed queue row: the cell geometry plus its pagination checkpoint.
#[derive(Debug)]
pub struct ClaimedCell {
    pub cell: Cell,
    pub next_skip: i64,
    pub total_count: Option<i64>,
}

/// One stage of the crack problem: identity + hashes + multichoice
/// options. Text material lives in CorpusRow, loaded in chunks — the
/// full set doesn't fit in RAM on small hosts.
#[derive(Debug)]
pub struct TargetStage {
    pub adventure_guid: String,
    pub stage_index: i64,
    /// answerCodeHashesBase16v2, falling back to [findCodeHashBase16V2].
    pub hashes: Vec<String>,
    /// multiChoiceOptions texts (empty for SingleChoice).
    pub options: Vec<String>,
}

/// Text fields one stage contributes to corpus mining.
#[derive(Debug)]
pub struct CorpusRow {
    pub title: String,
    pub question: String,
    pub description: String,
    pub adv_title: String,
    pub adv_description: String,
    pub options: Vec<String>,
}

/// A recovered answer for one (stage, hash) pair.
#[derive(Debug)]
pub struct CrackHit {
    pub adventure_guid: String,
    pub stage_index: i64,
    pub hash: String,
    pub plaintext: String,
    pub display: Option<String>,
    pub method: String,
}

/// One catalog/export row: a lab and its detail payload when fetched.
pub struct CatalogRow {
    pub guid: String,
    pub lab_json: Option<String>,
    pub adv_json: Option<String>,
    pub owner_username: Option<String>,
    pub reviews_total_count: Option<i64>,
}

#[derive(Default)]
pub struct Stats {
    pub pending: u64,
    pub in_progress: u64,
    pub done: u64,
    pub subdivided: u64,
    pub failed: u64,
    pub labs: u64,
    pub adventures_done: u64,
    pub adventures_failed: u64,
    pub stages: u64,
    pub reviews: u64,
    pub reviews_pending: u64,
    pub cracks: u64,
    pub cracked_stages: u64,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "queue: {} pending, {} in_progress, {} done, {} subdivided, {} failed | \
             labs: {} | adventures: {} ok, {} failed | stages: {} | \
             reviews: {} ({} adventures pending) | cracks: {} answers on {} stages",
            self.pending, self.in_progress, self.done, self.subdivided, self.failed,
            self.labs, self.adventures_done, self.adventures_failed, self.stages,
            self.reviews, self.reviews_pending, self.cracks, self.cracked_stages,
        )
    }
}

#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

/// Add a column to an existing table if it isn't there yet.
fn ensure_column(c: &Connection, table: &str, col: &str, ddl: &str) -> Result<()> {
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    if !names.iter().any(|n| n == col) {
        c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"))?;
    }
    Ok(())
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(60))?;
        conn.execute_batch(SCHEMA)?;
        ensure_column(&conn, "queue", "next_skip", "next_skip INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&conn, "adventures", "reviews_done", "reviews_done INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&conn, "adventures", "reviews_error", "reviews_error TEXT")?;
        // Never-delete history columns. Deliberately nullable with no
        // default: SQLite then skips the table rewrite, so migrating the
        // 25G database is instant. NULL status reads as 'active'.
        ensure_column(&conn, "adventures", "status", "status TEXT")?;
        ensure_column(&conn, "adventures", "content_hash", "content_hash TEXT")?;
        ensure_column(&conn, "adventures", "removed_at", "removed_at TEXT")?;
        ensure_column(&conn, "adventures", "version_seq", "version_seq INTEGER")?;
        ensure_column(&conn, "adventures", "reviews_checked_at", "reviews_checked_at TEXT")?;
        ensure_column(&conn, "stages", "status", "status TEXT")?;
        ensure_column(&conn, "stages", "content_hash", "content_hash TEXT")?;
        ensure_column(&conn, "stages", "removed_at", "removed_at TEXT")?;
        ensure_column(&conn, "stages", "version_seq", "version_seq INTEGER")?;
        ensure_column(&conn, "reviews", "status", "status TEXT")?;
        ensure_column(&conn, "reviews", "content_hash", "content_hash TEXT")?;
        ensure_column(&conn, "reviews", "removed_at", "removed_at TEXT")?;
        ensure_column(&conn, "reviews", "version_seq", "version_seq INTEGER")?;
        Ok(Self { inner: Arc::new(Mutex::new(conn)) })
    }

    /// Run a blocking DB op on the blocking thread pool.
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        spawn_blocking(move || {
            let mut conn = inner.lock().unwrap();
            f(&mut conn)
        })
        .await
        .context("db task join")?
    }

    // ── crawl queue ─────────────────────────────────────────────────────

    pub async fn reset(&self) -> Result<()> {
        self.run(|c| {
            c.execute_batch("DELETE FROM queue; DELETE FROM labs;")?;
            Ok(())
        })
        .await
    }

    pub async fn reset_failed(&self) -> Result<u64> {
        self.run(|c| {
            let n = c.execute(
                "UPDATE queue SET status='pending', updated_at=CURRENT_TIMESTAMP \
                 WHERE status='failed'",
                [],
            )?;
            Ok(n as u64)
        })
        .await
    }

    /// Reset in_progress cells whose claim went stale (crashed worker).
    /// Their next_skip checkpoints survive — they resume mid-cell.
    pub async fn reap_stale(&self, max_age_secs: i64) -> Result<u64> {
        self.run(move |c| {
            let n = c.execute(
                "UPDATE queue SET status='pending' WHERE status='in_progress' \
                 AND (strftime('%s','now') - strftime('%s', updated_at)) > ?1",
                params![max_age_secs],
            )?;
            Ok(n as u64)
        })
        .await
    }

    pub async fn seed(&self, cells: &[Cell]) -> Result<()> {
        let cells = cells.to_vec();
        self.run(move |c| {
            let tx = c.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO queue (id, lat, lon, radius, status) \
                     VALUES (?1, ?2, ?3, ?4, 'pending')",
                )?;
                for cell in &cells {
                    stmt.execute(params![cell.id, cell.lat, cell.lon, cell.radius])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn queue_is_empty(&self) -> Result<bool> {
        self.run(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM queue WHERE status IN ('pending','in_progress')",
                [],
                |r| r.get(0),
            )?;
            Ok(n == 0)
        })
        .await
    }

    /// Atomically reserve the largest-radius pending cell (breadth-first).
    pub async fn claim(&self) -> Result<Option<ClaimedCell>> {
        self.run(|c| {
            let row = c
                .query_row(
                    "UPDATE queue SET status='in_progress', attempts=attempts+1, \
                     updated_at=CURRENT_TIMESTAMP WHERE id = ( \
                       SELECT id FROM queue WHERE status='pending' \
                       ORDER BY radius DESC LIMIT 1 ) \
                     RETURNING id, lat, lon, radius, next_skip, total_count",
                    [],
                    |r| {
                        Ok(ClaimedCell {
                            cell: Cell {
                                id: r.get(0)?,
                                lat: r.get(1)?,
                                lon: r.get(2)?,
                                radius: r.get(3)?,
                            },
                            next_skip: r.get(4)?,
                            total_count: r.get(5)?,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    /// Hand a claim back to `pending` (graceful shutdown). next_skip is
    /// preserved, so the next claimant resumes mid-pagination.
    pub async fn release_claim(&self, id: String) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE queue SET status='pending', updated_at=CURRENT_TIMESTAMP \
                 WHERE id=?1 AND status='in_progress'",
                params![id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn in_progress_count(&self) -> Result<u64> {
        self.run(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM queue WHERE status='in_progress'",
                [],
                |r| r.get(0),
            )?;
            Ok(n as u64)
        })
        .await
    }

    /// Persist one fetched page: insert its items (deduped) and advance
    /// the cell's next_skip checkpoint — atomically, in one transaction.
    pub async fn record_page(
        &self,
        id: String,
        next_skip: i64,
        total_count: Option<i64>,
        items: Vec<Value>,
    ) -> Result<u64> {
        self.run(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE queue SET next_skip=?1, \
                 total_count=COALESCE(?2, total_count), \
                 updated_at=CURRENT_TIMESTAMP WHERE id=?3",
                params![next_skip, total_count, id],
            )?;
            let mut inserted = 0u64;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO labs (guid, raw_json, source) \
                     VALUES (?1, ?2, ?3)",
                )?;
                for item in &items {
                    let guid = item
                        .get("adventureGuid")
                        .or_else(|| item.get("id"))
                        .and_then(|g| g.as_str());
                    if let Some(guid) = guid {
                        let raw = serde_json::to_string(item)?;
                        inserted += stmt.execute(params![guid, raw, id])? as u64;
                    }
                }
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
    }

    /// Mark a cell fully processed. Items were already saved per-page.
    pub async fn mark_done(&self, id: String, total_count: Option<i64>) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE queue SET status='done', \
                 total_count=COALESCE(?1, total_count), \
                 updated_at=CURRENT_TIMESTAMP WHERE id=?2",
                params![total_count, id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn subdivide(&self, id: String, children: Vec<Cell>) -> Result<()> {
        self.run(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE queue SET status='subdivided', updated_at=CURRENT_TIMESTAMP \
                 WHERE id=?1",
                params![id],
            )?;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO queue (id, lat, lon, radius, status) \
                     VALUES (?1, ?2, ?3, ?4, 'pending')",
                )?;
                for cell in &children {
                    stmt.execute(params![cell.id, cell.lat, cell.lon, cell.radius])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Mark a cell failed. Its next_skip checkpoint survives — a later
    /// `--reset-failed` run resumes pagination where it died.
    pub async fn mark_failed(&self, id: String, reason: String) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE queue SET status='failed', updated_at=CURRENT_TIMESTAMP \
                 WHERE id=?1",
                params![id],
            )?;
            c.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![format!("failure:{id}"), reason],
            )?;
            Ok(())
        })
        .await
    }

    // ── meta ────────────────────────────────────────────────────────────

    pub async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.run(move |c| {
            Ok(c.query_row(
                "SELECT value FROM meta WHERE key=?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
        .await
    }

    pub async fn set_meta(&self, key: &str, value: String) -> Result<()> {
        let key = key.to_string();
        self.run(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn del_meta(&self, key: &str) -> Result<()> {
        let key = key.to_string();
        self.run(move |c| {
            c.execute("DELETE FROM meta WHERE key=?1", params![key])?;
            Ok(())
        })
        .await
    }

    pub async fn labs_count(&self) -> Result<u64> {
        self.run(|c| {
            Ok(c.query_row("SELECT COUNT(*) FROM labs", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
        })
        .await
    }

    // ── fetch stage ─────────────────────────────────────────────────────

    /// GUIDs still needing a detail fetch. With `require_hashes`, rows
    /// whose stored JSON lacks the answer-hash fields are re-fetched too.
    pub async fn pending_guids(&self, require_hashes: bool) -> Result<Vec<String>> {
        self.run(move |c| {
            let sql = if require_hashes {
                "SELECT l.guid FROM labs l LEFT JOIN adventures a ON a.guid = l.guid \
                 WHERE a.guid IS NULL OR a.http_status <> 200 OR a.error IS NOT NULL \
                    OR a.raw_json NOT LIKE '%findCodeHashBase16v2%' \
                    OR a.raw_json NOT LIKE '%answerCodeHashesBase16v2%'"
            } else {
                "SELECT l.guid FROM labs l LEFT JOIN adventures a ON a.guid = l.guid \
                 WHERE a.guid IS NULL OR a.http_status <> 200 OR a.error IS NOT NULL"
            };
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for g in rows {
                out.push(g?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn save_adventure(
        &self,
        guid: String,
        detail: Option<Value>,
        status: u16,
        error: Option<String>,
    ) -> Result<()> {
        self.run(move |c| {
            let tx = c.transaction()?;
            // Current live state, if any.
            let live: Option<(Option<String>, Option<i64>, Option<String>)> = tx
                .query_row(
                    "SELECT content_hash, http_status, status FROM adventures WHERE guid=?1",
                    params![guid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match (live, detail) {
                // First sighting and it errored: record an unfetched stub.
                // Never tombstones — there is no last-good state to lose.
                (None, None) => {
                    tx.execute(
                        "INSERT INTO adventures (guid, raw_json, http_status, error, status) \
                         VALUES (?1, '{}', ?2, ?3, 'unfetched')",
                        params![guid, status as i64, error],
                    )?;
                }
                // First good sighting: plain insert. This snapshot IS v1,
                // so no version row.
                (None, Some(d)) => {
                    Self::insert_adventure(&tx, &guid, &d, status)?;
                }
                (Some((_, prev_http, prev_status)), None) => {
                    let was_good = prev_http == Some(200);
                    let active = prev_status.as_deref().unwrap_or("active") == "active";
                    if status == 404 && was_good && active {
                        // Tombstone: archive last-good, mark removed, keep
                        // the data. A later 200 restores it (see below).
                        Self::archive_adventure(&tx, &guid, "removed")?;
                        tx.execute(
                            "UPDATE adventures SET status='removed', \
                             removed_at=CURRENT_TIMESTAMP, http_status=404, error=?2, \
                             fetched_at=CURRENT_TIMESTAMP, \
                             version_seq=COALESCE(version_seq,1)+1 WHERE guid=?1",
                            params![guid, error],
                        )?;
                        // Stages belong to the listing: tombstone them too.
                        // Reviews are standalone records; they stay.
                        Self::tombstone_stages(&tx, &guid)?;
                    } else {
                        // Transient failure (429/5xx/exhausted retries) or
                        // a row that never had good data: record the error,
                        // preserve everything else, no version row.
                        tx.execute(
                            "UPDATE adventures SET http_status=?2, error=?3, \
                             fetched_at=CURRENT_TIMESTAMP WHERE guid=?1",
                            params![guid, status as i64, error],
                        )?;
                    }
                }
                (Some((prev_hash, _, prev_status)), Some(d)) => {
                    let raw = serde_json::to_string(&d)?;
                    let hash = content_hash(&raw);
                    let restored = prev_status.as_deref().unwrap_or("active") == "removed";
                    // NULL hash = pre-history row: adopt as baseline with
                    // no archive, else the first refresh would double the DB.
                    let changed =
                        matches!(&prev_hash, Some(h) if *h != hash);
                    if restored || changed {
                        Self::archive_adventure(
                            &tx,
                            &guid,
                            if restored { "restored" } else { "updated" },
                        )?;
                        Self::update_adventure(&tx, &guid, &d, status, &hash)?;
                    } else {
                        // Same content, still active: light touch.
                        tx.execute(
                            "UPDATE adventures SET http_status=?2, error=NULL, \
                             fetched_at=CURRENT_TIMESTAMP, content_hash=?3 \
                             WHERE guid=?1",
                            params![guid, status as i64, hash],
                        )?;
                    }
                    Self::reconcile_stages(&tx, &guid, &d)?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    // ── reviews stage ───────────────────────────────────────────────────

// ── never-delete versioning helpers ─────────────────────────────────────
// Live tables always hold the latest snapshot; these archive the
// superseded row into the matching *_versions table first.

/// Snapshot the current live adventure row. Caller then updates live.
fn archive_adventure(tx: &Transaction, guid: &str, change: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO adventure_versions \
           (adventure_guid, version_seq, change, status, http_status, raw_json) \
         SELECT ?1, COALESCE(version_seq,1), ?2, \
           COALESCE(status,'active'), http_status, raw_json \
         FROM adventures WHERE guid=?1",
        params![guid, change],
    )?;
    Ok(())
}

/// Fresh insert of a first-seen adventure (v1, no version row).
fn insert_adventure(tx: &Transaction, guid: &str, d: &Value, status: u16) -> Result<()> {
    let (lat, lon) = loc(d);
    let themes = d.get("adventureThemes").cloned().unwrap_or(Value::Null);
    let raw = serde_json::to_string(d)?;
    let hash = content_hash(&raw);
    tx.execute(
        "INSERT INTO adventures (guid, title, description, adventure_type, \
           median_time_to_complete, ratings_average, ratings_total_count, \
           reviews_total_count, completion_count, recommended_count, \
           journals_total_count, completed_stages_count, stages_total_count, \
           owner_username, owner_public_guid, is_archived, is_test, \
           is_highly_recommended, visibility, published_utc, created_utc, \
           location_lat, location_lon, custom_access_code, themes_json, \
           raw_json, http_status, error, status, content_hash, version_seq) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,NULL,'active',?28,1)",
        params![
            guid,
            s(d, "title"),
            s(d, "description"),
            s(d, "adventureType"),
            i(d, "medianTimeToComplete"),
            f(d, "ratingsAverage"),
            i(d, "ratingsTotalCount"),
            i(d, "reviewsTotalCount"),
            i(d, "completionCount"),
            i(d, "recommendedCount"),
            i(d, "journalsTotalCount"),
            i(d, "completedStagesCount"),
            i(d, "stagesTotalCount"),
            s(d, "ownerUsername"),
            s(d, "ownerPublicGuid"),
            b(d, "isArchived"),
            b(d, "isTest"),
            b(d, "isHighlyRecommended"),
            s(d, "visibility"),
            s(d, "publishedUtc"),
            s(d, "createdUtc"),
            lat,
            lon,
            s(d, "customAccessCode"),
            serde_json::to_string(&themes)?,
            raw,
            status as i64,
            hash,
        ],
    )?;
    Self::reconcile_stages(tx, guid, d)?;
    Ok(())
}

/// Overwrite live with new content after archiving. Resets tombstones.
fn update_adventure(
    tx: &Transaction,
    guid: &str,
    d: &Value,
    status: u16,
    hash: &str,
) -> Result<()> {
    let (lat, lon) = loc(d);
    let themes = d.get("adventureThemes").cloned().unwrap_or(Value::Null);
    let raw = serde_json::to_string(d)?;
    tx.execute(
        "UPDATE adventures SET title=?2, description=?3, adventure_type=?4, \
           median_time_to_complete=?5, ratings_average=?6, ratings_total_count=?7, \
           reviews_total_count=?8, completion_count=?9, recommended_count=?10, \
           journals_total_count=?11, completed_stages_count=?12, \
           stages_total_count=?13, owner_username=?14, owner_public_guid=?15, \
           is_archived=?16, is_test=?17, is_highly_recommended=?18, \
           visibility=?19, published_utc=?20, created_utc=?21, \
           location_lat=?22, location_lon=?23, custom_access_code=?24, \
           themes_json=?25, raw_json=?26, http_status=?27, error=NULL, \
           status='active', removed_at=NULL, content_hash=?28, \
           fetched_at=CURRENT_TIMESTAMP, version_seq=COALESCE(version_seq,1)+1 \
         WHERE guid=?1",
        params![
            guid,
            s(d, "title"),
            s(d, "description"),
            s(d, "adventureType"),
            i(d, "medianTimeToComplete"),
            f(d, "ratingsAverage"),
            i(d, "ratingsTotalCount"),
            i(d, "reviewsTotalCount"),
            i(d, "completionCount"),
            i(d, "recommendedCount"),
            i(d, "journalsTotalCount"),
            i(d, "completedStagesCount"),
            i(d, "stagesTotalCount"),
            s(d, "ownerUsername"),
            s(d, "ownerPublicGuid"),
            b(d, "isArchived"),
            b(d, "isTest"),
            b(d, "isHighlyRecommended"),
            s(d, "visibility"),
            s(d, "publishedUtc"),
            s(d, "createdUtc"),
            lat,
            lon,
            s(d, "customAccessCode"),
            serde_json::to_string(&themes)?,
            raw,
            status as i64,
            hash,
        ],
    )?;
    Ok(())
}

/// Reconcile live stages against a fresh detail listing: insert new,
/// version-and-update changed, tombstone vanished. Never hard-deletes.
fn reconcile_stages(tx: &Transaction, guid: &str, d: &Value) -> Result<()> {
    let stages = d
        .get("stageSummaries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
        // (status, content_hash) for every known stage, active or not, so
        // reappearing stages restore instead of conflicting on INSERT.
        let mut live: std::collections::HashMap<i64, (String, Option<String>)> =
            std::collections::HashMap::new();
        let mut stmt = tx.prepare(
            "SELECT stage_index, COALESCE(status,'active'), content_hash FROM stages \
             WHERE adventure_guid=?1",
        )?;
        for row in stmt.query_map(params![guid], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        })? {
            let (idx, st, h) = row?;
            live.insert(idx, (st, h));
        }
        let mut ins = tx.prepare(
            "INSERT INTO stages (adventure_guid, stage_index, title, \
               description, challenge_type, is_complete, is_final, \
               geofencing_radius, latitude, longitude, question, raw_json, \
               status, content_hash, version_seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'active',?13,1)",
        )?;
        for (idx, stage) in stages.iter().enumerate() {
            let idx = idx as i64;
            let raw = serde_json::to_string(stage)?;
            let hash = content_hash(&raw);
            match live.remove(&idx) {
                // Brand new stage.
                None => {
                    let (slat, slon) = loc(stage);
                    ins.execute(params![
                        guid,
                        idx,
                        s(stage, "title"),
                        s(stage, "description"),
                        s(stage, "challengeType"),
                        b(stage, "isComplete"),
                        b(stage, "isFinal"),
                        i(stage, "geofencingRadius"),
                        slat,
                        slon,
                        s(stage, "question"),
                        raw,
                        hash,
                    ])?;
                }
                // Unchanged active stage: leave it alone entirely.
                Some((st, Some(h))) if st == "active" && h == hash => {}
                // Known stage: archive on real change (not on baseline
                // adoption, not on restore — that snapshot is already in
                // versions from tombstone time), then overwrite live.
                Some((st, prev_hash)) => {
                    let changed = matches!(&prev_hash, Some(h) if *h != hash);
                    if changed && st == "active" {
                        tx.execute(
                            "INSERT INTO stage_versions \
                               (adventure_guid, stage_index, version_seq, change, raw_json) \
                             SELECT adventure_guid, stage_index, \
                               COALESCE(version_seq,1), 'updated', raw_json \
                             FROM stages WHERE adventure_guid=?1 AND stage_index=?2",
                            params![guid, idx],
                        )?;
                    }
                    let (slat, slon) = loc(stage);
                    tx.execute(
                        "UPDATE stages SET title=?3, description=?4, \
                         challenge_type=?5, is_complete=?6, is_final=?7, \
                         geofencing_radius=?8, latitude=?9, longitude=?10, \
                         question=?11, raw_json=?12, content_hash=?13, \
                         status='active', removed_at=NULL, \
                         version_seq=COALESCE(version_seq,1)+1 WHERE \
                         adventure_guid=?1 AND stage_index=?2",
                        params![
                            guid,
                            idx,
                            s(stage, "title"),
                            s(stage, "description"),
                            s(stage, "challengeType"),
                            b(stage, "isComplete"),
                            b(stage, "isFinal"),
                            i(stage, "geofencingRadius"),
                            slat,
                            slon,
                            s(stage, "question"),
                            raw,
                            hash,
                        ],
                    )?;
                }
            }
        }
    // Anything left in `live` vanished from the listing: tombstone it.
    drop(ins);
    for idx in live.into_keys() {
        tx.execute(
            "INSERT INTO stage_versions \
               (adventure_guid, stage_index, version_seq, change, raw_json) \
             SELECT adventure_guid, stage_index, COALESCE(version_seq,1), \
               'deleted', raw_json \
             FROM stages WHERE adventure_guid=?1 AND stage_index=?2",
            params![guid, idx],
        )?;
        tx.execute(
            "UPDATE stages SET status='removed', removed_at=CURRENT_TIMESTAMP, \
             version_seq=COALESCE(version_seq,1)+1 \
             WHERE adventure_guid=?1 AND stage_index=?2",
            params![guid, idx],
        )?;
    }
    Ok(())
}

/// Tombstone every active stage of an adventure (parent got 404).
fn tombstone_stages(tx: &Transaction, guid: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO stage_versions \
           (adventure_guid, stage_index, version_seq, change, raw_json) \
         SELECT adventure_guid, stage_index, COALESCE(version_seq,1), \
           'removed', raw_json FROM stages \
         WHERE adventure_guid=?1 AND COALESCE(status,'active')='active'",
        params![guid],
    )?;
    tx.execute(
        "UPDATE stages SET status='removed', removed_at=CURRENT_TIMESTAMP, \
         version_seq=COALESCE(version_seq,1)+1 \
         WHERE adventure_guid=?1 AND COALESCE(status,'active')='active'",
        params![guid],
    )?;
    Ok(())
}

    /// Adventures whose detail shows reviews and haven't been scraped.
    pub async fn pending_review_guids(&self) -> Result<Vec<String>> {
        self.run(|c| {
            let mut stmt = c.prepare(
                "SELECT guid FROM adventures WHERE http_status=200 \
                 AND COALESCE(reviews_total_count,0) > 0 AND reviews_done=0",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for g in rows {
                out.push(g?);
            }
            Ok(out)
        })
        .await
    }

    /// Persist one adventure's review pages + flip reviews_done — one tx.
    /// Never-delete: changed reviews archive the old row, vanished ones
    /// are tombstoned (each processed adventure is a full listing, so
    /// absence is conclusive), reappeared ones restore. Unchanged rows
    /// are not touched at all.
    pub async fn save_reviews(
        &self,
        guid: String,
        total_count: i64,
        items: Vec<Value>,
    ) -> Result<u64> {
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut written = 0u64;
            tx.execute("CREATE TEMP TABLE seen_ids(id INTEGER PRIMARY KEY)", [])?;
            {
                let mut known: std::collections::HashMap<i64, (String, Option<String>)> =
                    std::collections::HashMap::new();
                let mut stmt = tx.prepare(
                    "SELECT id, COALESCE(status,'active'), content_hash FROM reviews \
                     WHERE adventure_guid=?1",
                )?;
                for row in stmt.query_map(params![guid], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })? {
                    let (id, st, h) = row?;
                    known.insert(id, (st, h));
                }
                let mut ins = tx.prepare(
                    "INSERT INTO reviews (id, adventure_guid, rating, \
                       review_text, player_username, player_public_guid, \
                       player_geocache_find_count, player_completed_adventure_count, \
                       recommended, is_creator, created_utc, completed_utc, \
                       images_json, tags_json, raw_json, status, content_hash, \
                       version_seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15, \
                       'active',?16,1)",
                )?;
                let mut seen_ins =
                    tx.prepare("INSERT OR IGNORE INTO seen_ids(id) VALUES (?1)")?;
                for r in &items {
                    let Some(id) = i(r, "id") else { continue };
                    seen_ins.execute(params![id])?;
                    let images = r.get("images").cloned().unwrap_or(Value::Null);
                    let tags = r.get("playerTags").cloned().unwrap_or(Value::Null);
                    let raw = serde_json::to_string(r)?;
                    let hash = content_hash(&raw);
                    match known.remove(&id) {
                        None => {
                            ins.execute(params![
                                id,
                                guid,
                                i(r, "rating"),
                                s(r, "reviewText"),
                                s(r, "playerUsername"),
                                s(r, "playerPublicGuid"),
                                i(r, "playerGeocacheFindCount"),
                                i(r, "playerCompletedAdventureCount"),
                                b(r, "recommended"),
                                b(r, "isCreator"),
                                s(r, "createdUtc"),
                                s(r, "adventureCompletedDateUtc"),
                                serde_json::to_string(&images)?,
                                serde_json::to_string(&tags)?,
                                raw,
                                hash,
                            ])?;
                            written += 1;
                        }
                        Some((st, Some(h))) if st == "active" && h == hash => {}
                        Some((st, prev_hash)) => {
                            let changed =
                                matches!(&prev_hash, Some(h) if *h != hash);
                            if changed && st == "active" {
                                tx.execute(
                                    "INSERT INTO review_versions \
                                       (review_id, version_seq, change, raw_json) \
                                     SELECT id, COALESCE(version_seq,1), 'updated', raw_json \
                                     FROM reviews WHERE id=?1",
                                    params![id],
                                )?;
                            }
                            tx.execute(
                                "UPDATE reviews SET adventure_guid=?2, rating=?3, \
                                 review_text=?4, player_username=?5, player_public_guid=?6, \
                                 player_geocache_find_count=?7, \
                                 player_completed_adventure_count=?8, recommended=?9, \
                                 is_creator=?10, created_utc=?11, completed_utc=?12, \
                                 images_json=?13, tags_json=?14, raw_json=?15, \
                                 status='active', removed_at=NULL, content_hash=?16, \
                                 fetched_at=CURRENT_TIMESTAMP, \
                                 version_seq=COALESCE(version_seq,1)+1 WHERE id=?1",
                                params![
                                    id,
                                    guid,
                                    i(r, "rating"),
                                    s(r, "reviewText"),
                                    s(r, "playerUsername"),
                                    s(r, "playerPublicGuid"),
                                    i(r, "playerGeocacheFindCount"),
                                    i(r, "playerCompletedAdventureCount"),
                                    b(r, "recommended"),
                                    b(r, "isCreator"),
                                    s(r, "createdUtc"),
                                    s(r, "adventureCompletedDateUtc"),
                                    serde_json::to_string(&images)?,
                                    serde_json::to_string(&tags)?,
                                    raw,
                                    hash,
                                ],
                            )?;
                            written += 1;
                        }
                    }
                }
            }
            // Reviews in the DB but absent from this full listing were
            // deleted (or made private): archive + tombstone, keep data.
            tx.execute(
                "INSERT INTO review_versions \
                   (review_id, version_seq, change, raw_json) \
                 SELECT id, COALESCE(version_seq,1), 'removed', raw_json FROM reviews \
                 WHERE adventure_guid=?1 AND COALESCE(status,'active')='active' \
                   AND id NOT IN (SELECT id FROM seen_ids)",
                params![guid],
            )?;
            tx.execute(
                "UPDATE reviews SET status='removed', removed_at=CURRENT_TIMESTAMP, \
                 version_seq=COALESCE(version_seq,1)+1 \
                 WHERE adventure_guid=?1 AND COALESCE(status,'active')='active' \
                   AND id NOT IN (SELECT id FROM seen_ids)",
                params![guid],
            )?;
            tx.execute(
                "UPDATE adventures SET reviews_done=1, reviews_error=NULL, \
                 reviews_checked_at=CURRENT_TIMESTAMP, \
                 reviews_total_count=MAX(COALESCE(reviews_total_count,0), ?2) \
                 WHERE guid=?1",
                params![guid, total_count],
            )?;
            tx.commit()?;
            Ok(written)
        })
        .await
    }

    /// Leave the adventure pending but record why it failed.
    pub async fn mark_reviews_failed(&self, guid: String, err: String) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE adventures SET reviews_error=?2 WHERE guid=?1",
                params![guid, err],
            )?;
            Ok(())
        })
        .await
    }

    // ── refresh mode ────────────────────────────────────────────────────

    /// Re-queue finished cells for a refresh pass: done/failed cells go
    /// back to pending from offset 0. Subdivided parents stay put (their
    /// children cover the area). Discovery inserts are idempotent
    /// (`labs.guid` dedup), so re-walking only adds newly-appeared labs.
    pub async fn requeue_done(&self) -> Result<u64> {
        self.run(|c| {
            Ok(c.execute(
                "UPDATE queue SET status='pending', next_skip=0, attempts=0, \
                 updated_at=CURRENT_TIMESTAMP WHERE status IN ('done','failed')",
                [],
            )? as u64)
        })
        .await
    }

    /// Stalest-first detail re-fetch candidates (refresh mode). -1 = all.
    /// Only live, previously-good rows: errors/unfetched stay in the
    /// `fetch` (pending) lane, removed rows stay tombstoned until a
    /// revisit proves otherwise — which this also does, since a restored
    /// adventure answers 200 again and `save_adventure` revives it.
    pub async fn refetch_guids(&self, limit: i64) -> Result<Vec<String>> {
        self.run(move |c| {
            let mut stmt = c.prepare(
                "SELECT guid FROM adventures \
                 WHERE COALESCE(status,'active')='active' \
                 ORDER BY fetched_at ASC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for g in rows {
                out.push(g?);
            }
            Ok(out)
        })
        .await
    }

    /// Stalest-first reviews re-scrape candidates (refresh mode). -1 = all.
    /// `reviews_checked_at` advances on every completed scrape, so each
    /// pass works through the least-recently-checked adventures first.
    pub async fn revisit_review_guids(&self, limit: i64) -> Result<Vec<String>> {
        self.run(move |c| {
            let mut stmt = c.prepare(
                "SELECT guid FROM adventures \
                 WHERE http_status=200 AND COALESCE(status,'active')='active' \
                   AND reviews_done=1 AND COALESCE(reviews_total_count,0) > 0 \
                 ORDER BY COALESCE(reviews_checked_at,'1970-01-01') ASC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for g in rows {
                out.push(g?);
            }
            Ok(out)
        })
        .await
    }

    // ── crack stage ───────────────────────────────────────────────────

    /// Stages carrying answer hashes: identity + hashes + multichoice
    /// options via json_extract (raw_json never leaves SQLite). Light
    /// enough to hold for the whole run — corpus text is loaded
    /// separately in chunks via corpus_rows().
    pub async fn crack_targets(&self) -> Result<Vec<TargetStage>> {
        self.run(|c| {
            let mut stmt = c.prepare(
                "SELECT s.adventure_guid, s.stage_index, \
                 json_extract(s.raw_json,'$.answerCodeHashesBase16v2'), \
                 json_extract(s.raw_json,'$.findCodeHashBase16v2'), \
                 json_extract(s.raw_json,'$.multiChoiceOptions') \
                 FROM stages s \
                 WHERE s.raw_json LIKE '%findCodeHashBase16v2%'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (ag, si, hashes_j, find_h, opts_j) = r?;
                let mut hashes: Vec<String> = hashes_j
                    .and_then(|j| serde_json::from_str(&j).ok())
                    .unwrap_or_default();
                if hashes.is_empty() {
                    if let Some(h) = find_h {
                        hashes.push(h);
                    }
                }
                if hashes.is_empty() {
                    continue;
                }
                let options: Vec<String> = opts_j
                    .and_then(|j| serde_json::from_str::<Vec<Value>>(&j).ok())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|o| s(o, "text"))
                    .collect();
                out.push(TargetStage {
                    adventure_guid: ag,
                    stage_index: si,
                    hashes,
                    options,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Stages carrying answer hashes (for corpus chunk sizing).
    pub async fn corpus_row_count(&self) -> Result<i64> {
        self.run(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM stages \
                 WHERE raw_json LIKE '%findCodeHashBase16v2%'",
                [],
                |r| r.get(0),
            )?;
            Ok(n)
        })
        .await
    }

    /// One chunk of corpus-mining text, ordered for stable pagination.
    pub async fn corpus_rows(&self, offset: i64, limit: i64) -> Result<Vec<CorpusRow>> {
        self.run(move |c| {
            let mut stmt = c.prepare(
                "SELECT COALESCE(s.title,''), COALESCE(s.question,''), \
                 COALESCE(s.description,''), \
                 json_extract(s.raw_json,'$.multiChoiceOptions'), \
                 COALESCE(a.title,''), COALESCE(a.description,'') \
                 FROM stages s JOIN adventures a ON a.guid = s.adventure_guid \
                 WHERE s.raw_json LIKE '%findCodeHashBase16v2%' \
                 ORDER BY s.adventure_guid, s.stage_index \
                 LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit, offset], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (t, q, d, opts_j, at, ad) = r?;
                let options: Vec<String> = opts_j
                    .and_then(|j| serde_json::from_str::<Vec<Value>>(&j).ok())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|o| s(o, "text"))
                    .collect();
                out.push(CorpusRow {
                    title: t,
                    question: q,
                    description: d,
                    adv_title: at,
                    adv_description: ad,
                    options,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Persist recovered answers. Idempotent — re-running a phase is a
    /// no-op via the (adventure_guid, stage_index, hash) primary key.
    pub async fn save_cracks(&self, hits: Vec<CrackHit>) -> Result<u64> {
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut inserted = 0u64;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO cracks (adventure_guid, stage_index, \
                     hash, plaintext, display, method) VALUES (?1,?2,?3,?4,?5,?6)",
                )?;
                for h in &hits {
                    inserted += stmt.execute(params![
                        h.adventure_guid,
                        h.stage_index,
                        h.hash,
                        h.plaintext,
                        h.display,
                        h.method
                    ])? as u64;
                }
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
    }

    /// (answer rows, distinct stages with >= 1 cracked answer).
    pub async fn crack_summary(&self) -> Result<(u64, u64)> {
        self.run(|c| {
            let rows: i64 =
                c.query_row("SELECT COUNT(*) FROM cracks", [], |r| r.get(0))?;
            let stages: i64 = c.query_row(
                "SELECT COUNT(*) FROM (SELECT 1 FROM cracks \
                 GROUP BY adventure_guid, stage_index)",
                [],
                |r| r.get(0),
            )?;
            Ok((rows as u64, stages as u64))
        })
        .await
    }

    // ── export ──────────────────────────────────────────────────────────

    /// Catalog rows for labs whose guid starts with the given 2-hex-char
    /// prefix — keeps export peak memory near ~1/256 of the dataset.
    pub async fn catalog_rows_prefix(&self, prefix: String) -> Result<Vec<CatalogRow>> {
        self.run(move |c| {
            // Literal GLOB pattern (not a bound expr) so SQLite can use the
            // guid index for a range scan instead of a full-table scan.
            let mut stmt = c.prepare(&format!(
                "SELECT l.guid, l.raw_json, a.raw_json, a.owner_username, \
                 a.reviews_total_count FROM labs l \
                 LEFT JOIN adventures a ON a.guid = l.guid AND a.http_status = 200 \
                   AND COALESCE(a.status,'active') = 'active' \
                 WHERE l.guid GLOB '{prefix}*'",
            ))?;
            let rows = stmt.query_map([], |r| {
                Ok(CatalogRow {
                    guid: r.get(0)?,
                    lab_json: r.get(1)?,
                    adv_json: r.get(2)?,
                    owner_username: r.get(3)?,
                    reviews_total_count: r.get(4)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Up to `limit` reviews per adventure, grouped by adventure_guid,
    /// restricted to one 2-hex-char guid prefix. The viewer only needs a
    /// sample — shipping all reviews would make shards ~10x bigger.
    pub async fn reviews_prefix(
        &self,
        prefix: String,
        limit: usize,
    ) -> Result<std::collections::BTreeMap<String, Vec<Value>>> {
        self.run(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT adventure_guid, raw_json FROM reviews \
                 WHERE adventure_guid GLOB '{prefix}*' \
                   AND COALESCE(status,'active') = 'active' \
                 ORDER BY created_utc DESC",
            ))?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out: std::collections::BTreeMap<String, Vec<Value>> =
                std::collections::BTreeMap::new();
            for r in rows {
                let (g, j) = r?;
                let entry = out.entry(g).or_default();
                if entry.len() >= limit {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&j) {
                    entry.push(v);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Cracks for one 2-hex-char guid prefix.
    pub async fn cracks_prefix(
        &self,
        prefix: String,
    ) -> Result<std::collections::BTreeMap<String, Vec<(i64, String, Option<String>, String)>>>
    {
        self.run(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT adventure_guid, stage_index, plaintext, display, method \
                 FROM cracks WHERE adventure_guid GLOB '{prefix}*' \
                 ORDER BY adventure_guid, stage_index",
            ))?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;
            let mut out: std::collections::BTreeMap<
                String,
                Vec<(i64, String, Option<String>, String)>,
            > = std::collections::BTreeMap::new();
            for r in rows {
                let (g, si, p, d, m) = r?;
                out.entry(g).or_default().push((si, p, d, m));
            }
            Ok(out)
        })
        .await
    }

    // ── stats ───────────────────────────────────────────────────────────

    pub async fn stats(&self) -> Result<Stats> {
        self.run(|c| {
            let mut st = Stats::default();
            let mut q = c.prepare("SELECT status, COUNT(*) FROM queue GROUP BY status")?;
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (status, n) = row?;
                match status.as_str() {
                    "pending" => st.pending = n as u64,
                    "in_progress" => st.in_progress = n as u64,
                    "done" => st.done = n as u64,
                    "subdivided" => st.subdivided = n as u64,
                    "failed" => st.failed = n as u64,
                    _ => {}
                }
            }
            drop(q);
            st.labs = c.query_row("SELECT COUNT(*) FROM labs", [], |r| r.get::<_, i64>(0))? as u64;
            st.adventures_done = c.query_row(
                "SELECT COUNT(*) FROM adventures WHERE http_status=200 AND error IS NULL",
                [], |r| r.get::<_, i64>(0))? as u64;
            st.adventures_failed = c.query_row(
                "SELECT COUNT(*) FROM adventures WHERE error IS NOT NULL \
                 OR (http_status IS NOT NULL AND http_status <> 200)",
                [], |r| r.get::<_, i64>(0))? as u64;
            st.stages = c.query_row("SELECT COUNT(*) FROM stages", [], |r| r.get::<_, i64>(0))? as u64;
            st.reviews = c.query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get::<_, i64>(0))? as u64;
            st.reviews_pending = c.query_row(
                "SELECT COUNT(*) FROM adventures WHERE http_status=200 \
                 AND COALESCE(reviews_total_count,0) > 0 AND reviews_done=0",
                [], |r| r.get::<_, i64>(0))? as u64;
            st.cracks = c.query_row("SELECT COUNT(*) FROM cracks", [], |r| r.get::<_, i64>(0))? as u64;
            st.cracked_stages = c.query_row(
                "SELECT COUNT(*) FROM (SELECT 1 FROM cracks \
                 GROUP BY adventure_guid, stage_index)",
                [], |r| r.get::<_, i64>(0))? as u64;
            Ok(st)
        })
        .await
    }
}

// ── JSON extraction helpers ─────────────────────────────────────────────

use md5::{Digest, Md5};

/// Content hash for change detection (never-delete versioning). md5 is
/// fine here — this is a fingerprint, not security.
fn content_hash(raw: &str) -> String {
    format!("{:x}", Md5::digest(raw.as_bytes()))
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(String::from)
}

fn i(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_u64().map(|n| n as i64))
            .or_else(|| x.as_f64().map(|n| n as i64))
    })
}

fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn b(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_bool()).map(|x| x as i64)
}

fn loc(d: &Value) -> (Option<f64>, Option<f64>) {
    let l = d.get("location").cloned().unwrap_or(Value::Null);
    (f(&l, "latitude"), f(&l, "longitude"))
}
