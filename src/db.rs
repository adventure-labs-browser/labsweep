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
use rusqlite::{params, Connection, OptionalExtension};
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
    fetched_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_adv_pub ON adventures(published_utc);

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
    fetched_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_reviews_adv ON reviews(adventure_guid);

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
            match detail {
                None => {
                    tx.execute(
                        "INSERT INTO adventures (guid, raw_json, http_status, error) \
                         VALUES (?1, '{}', ?2, ?3) \
                         ON CONFLICT(guid) DO UPDATE SET raw_json=excluded.raw_json, \
                           http_status=excluded.http_status, error=excluded.error, \
                           fetched_at=CURRENT_TIMESTAMP",
                        params![guid, status as i64, error],
                    )?;
                }
                Some(d) => {
                    let (lat, lon) = loc(&d);
                    let themes = d.get("adventureThemes").cloned().unwrap_or(Value::Null);
                    tx.execute(
                        "INSERT INTO adventures (guid, title, description, adventure_type, \
                           median_time_to_complete, ratings_average, ratings_total_count, \
                           reviews_total_count, completion_count, recommended_count, \
                           journals_total_count, completed_stages_count, stages_total_count, \
                           owner_username, owner_public_guid, is_archived, is_test, \
                           is_highly_recommended, visibility, published_utc, created_utc, \
                           location_lat, location_lon, custom_access_code, themes_json, \
                           raw_json, http_status, error) \
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28) \
                         ON CONFLICT(guid) DO UPDATE SET \
                           title=excluded.title, description=excluded.description, \
                           adventure_type=excluded.adventure_type, \
                           median_time_to_complete=excluded.median_time_to_complete, \
                           ratings_average=excluded.ratings_average, \
                           ratings_total_count=excluded.ratings_total_count, \
                           reviews_total_count=excluded.reviews_total_count, \
                           completion_count=excluded.completion_count, \
                           recommended_count=excluded.recommended_count, \
                           journals_total_count=excluded.journals_total_count, \
                           completed_stages_count=excluded.completed_stages_count, \
                           stages_total_count=excluded.stages_total_count, \
                           owner_username=excluded.owner_username, \
                           owner_public_guid=excluded.owner_public_guid, \
                           is_archived=excluded.is_archived, is_test=excluded.is_test, \
                           is_highly_recommended=excluded.is_highly_recommended, \
                           visibility=excluded.visibility, \
                           published_utc=excluded.published_utc, \
                           created_utc=excluded.created_utc, \
                           location_lat=excluded.location_lat, \
                           location_lon=excluded.location_lon, \
                           custom_access_code=excluded.custom_access_code, \
                           themes_json=excluded.themes_json, raw_json=excluded.raw_json, \
                           http_status=excluded.http_status, error=excluded.error, \
                           fetched_at=CURRENT_TIMESTAMP",
                        params![
                            guid,
                            s(&d, "title"),
                            s(&d, "description"),
                            s(&d, "adventureType"),
                            i(&d, "medianTimeToComplete"),
                            f(&d, "ratingsAverage"),
                            i(&d, "ratingsTotalCount"),
                            i(&d, "reviewsTotalCount"),
                            i(&d, "completionCount"),
                            i(&d, "recommendedCount"),
                            i(&d, "journalsTotalCount"),
                            i(&d, "completedStagesCount"),
                            i(&d, "stagesTotalCount"),
                            s(&d, "ownerUsername"),
                            s(&d, "ownerPublicGuid"),
                            b(&d, "isArchived"),
                            b(&d, "isTest"),
                            b(&d, "isHighlyRecommended"),
                            s(&d, "visibility"),
                            s(&d, "publishedUtc"),
                            s(&d, "createdUtc"),
                            lat,
                            lon,
                            s(&d, "customAccessCode"),
                            serde_json::to_string(&themes)?,
                            serde_json::to_string(&d)?,
                            status as i64,
                            Option::<String>::None,
                        ],
                    )?;
                    tx.execute("DELETE FROM stages WHERE adventure_guid=?1", params![guid])?;
                    if let Some(stages) = d.get("stageSummaries").and_then(|v| v.as_array()) {
                        let mut stmt = tx.prepare(
                            "INSERT INTO stages (adventure_guid, stage_index, title, \
                               description, challenge_type, is_complete, is_final, \
                               geofencing_radius, latitude, longitude, question, raw_json) \
                             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                        )?;
                        for (idx, stage) in stages.iter().enumerate() {
                            let (slat, slon) = loc(stage);
                            stmt.execute(params![
                                guid,
                                idx as i64,
                                s(stage, "title"),
                                s(stage, "description"),
                                s(stage, "challengeType"),
                                b(stage, "isComplete"),
                                b(stage, "isFinal"),
                                i(stage, "geofencingRadius"),
                                slat,
                                slon,
                                s(stage, "question"),
                                serde_json::to_string(stage)?,
                            ])?;
                        }
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    // ── reviews stage ───────────────────────────────────────────────────

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
    pub async fn save_reviews(
        &self,
        guid: String,
        total_count: i64,
        items: Vec<Value>,
    ) -> Result<u64> {
        self.run(move |c| {
            let tx = c.transaction()?;
            let mut inserted = 0u64;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR REPLACE INTO reviews (id, adventure_guid, rating, \
                       review_text, player_username, player_public_guid, \
                       player_geocache_find_count, player_completed_adventure_count, \
                       recommended, is_creator, created_utc, completed_utc, \
                       images_json, tags_json, raw_json) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                )?;
                for r in &items {
                    let Some(id) = i(r, "id") else { continue };
                    let images = r.get("images").cloned().unwrap_or(Value::Null);
                    let tags = r.get("playerTags").cloned().unwrap_or(Value::Null);
                    inserted += stmt.execute(params![
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
                        serde_json::to_string(r)?,
                    ])? as u64;
                }
            }
            tx.execute(
                "UPDATE adventures SET reviews_done=1, reviews_error=NULL, \
                 reviews_total_count=MAX(COALESCE(reviews_total_count,0), ?2) \
                 WHERE guid=?1",
                params![guid, total_count],
            )?;
            tx.commit()?;
            Ok(inserted)
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
