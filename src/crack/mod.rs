//! Stage 4: crack stage answer hashes.
//!
//! Algorithm (byte-exact from the app's own validator —
//! com.groundspeak.react.adventures v1.71.0, Hermes bundle):
//!
//!   md5(playerPublicGuid + normalize(input)) ∈ answerCodeHashesBase16V2
//!
//! The salt is the *fetching account's* publicGuid (meta.auth.public_guid).
//! Every hash in the db shares it, so each generated candidate costs one
//! md5 and is checked against ALL stage hashes at once — the attack is
//! O(candidates), never O(candidates x stages).
//!
//!   normalize/  formatAnswerInput + md5 port
//!   gen/        candidate generators (multichoice, numeric, corpus, brute)
//!   verify/     self-test on known MultiChoice options before spending cpu
//!   hashcat/    export `hash:salt` (mode 20) / import potfile hits

mod gen;
mod hashcat;
mod normalize;
mod verify;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::auth::AuthState;
use crate::db::{CrackHit, Db, TargetStage};

use gen::key;

/// Corpus text rows per fetch — keeps peak memory small on tiny hosts.
const CORPUS_CHUNK: i64 = 50_000;

pub use normalize::{answer_hash, normalize_answer};

/// hash -> every (adventure_guid, stage_index) accepting it as an answer.
type Targets = HashMap<String, Vec<(String, i64)>>;

#[derive(Default)]
pub struct Args {
    pub phases: String,
    pub wordlist: Option<PathBuf>,
    pub brute_len: usize,
    pub export_hashes: Option<PathBuf>,
    pub import_hashes: Option<PathBuf>,
    pub salt: Option<String>,
}

fn build_targets(stages: &[TargetStage]) -> Targets {
    let mut t: Targets = HashMap::new();
    for s in stages {
        for h in &s.hashes {
            t.entry(h.clone())
                .or_default()
                .push((s.adventure_guid.clone(), s.stage_index));
        }
    }
    t
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    let salt = match &args.salt {
        Some(s) => s.clone(),
        None => {
            let raw = db
                .get_meta("auth")
                .await?
                .context("no stored auth — `labsweep auth login` first (public_guid is the salt)")?;
            serde_json::from_str::<AuthState>(&raw)?
                .public_guid
                .context("stored auth has no public_guid")?
        }
    };
    info!("crack salt: {salt}");

    let stages = db.crack_targets().await?;
    if stages.is_empty() {
        info!("no stages carrying answer hashes yet");
        return Ok(());
    }
    let targets = build_targets(&stages);
    info!(
        "{} stages carrying {} unique target hashes",
        stages.len(),
        targets.len()
    );

    // standalone file operations exit before the compute phases
    if let Some(p) = &args.export_hashes {
        let n = hashcat::export(p, &salt, &targets)?;
        info!("wrote {n} hash:salt lines to {}", p.display());
        return Ok(());
    }
    if let Some(p) = &args.import_hashes {
        let hits = hashcat::import(p, &targets)?;
        let n = db.save_cracks(hits).await?;
        info!("imported {n} hashcat cracks");
        return Ok(());
    }

    verify::self_test(&salt, &stages)?;

    let mut seen: HashSet<u64> = HashSet::new();
    for ph in args.phases.split(',').map(str::trim) {
        match ph {
            "multichoice" => {
                phase(&db, &salt, &targets, &mut seen, "multichoice",
                      gen::multichoice(&stages))
                    .await?;
            }
            "numeric" => {
                phase(&db, &salt, &targets, &mut seen, "numeric", gen::numeric())
                    .await?;
            }
            "patterns" => {
                phase(&db, &salt, &targets, &mut seen, "patterns", gen::patterns())
                    .await?;
            }
            "mangle" => {
                let total = db.corpus_row_count().await?;
                let mut off = 0i64;
                while off < total {
                    let rows = db.corpus_rows(off, CORPUS_CHUNK).await?;
                    let n = rows.len() as i64;
                    if n == 0 {
                        break;
                    }
                    info!("mangle: rows {}..{} of {total}", off, off + n);
                    phase(&db, &salt, &targets, &mut seen, "mangle",
                          gen::mangle(&rows))
                        .await?;
                    off += n;
                }
            }
            "corpus" => {
                let total = db.corpus_row_count().await?;
                let mut off = 0i64;
                while off < total {
                    let rows = db.corpus_rows(off, CORPUS_CHUNK).await?;
                    let n = rows.len() as i64;
                    if n == 0 {
                        break;
                    }
                    info!("corpus: rows {}..{} of {total}", off, off + n);
                    phase(&db, &salt, &targets, &mut seen, "corpus",
                          gen::corpus(&rows))
                        .await?;
                    off += n;
                }
            }
            "wordlist" => {
                let p = args
                    .wordlist
                    .as_ref()
                    .context("--phases wordlist needs --wordlist PATH")?;
                phase(&db, &salt, &targets, &mut seen, "wordlist",
                      gen::wordlist(p)?)
                .await?;
            }
            "brute" => {} // runs below via --brute-len
            "" => {}
            other => warn!("unknown crack phase '{other}'"),
        }
    }

    if args.brute_len > 0 {
        brute_run(&db, &salt, &targets, args.brute_len).await?;
    }

    let (answers, nstages) = db.crack_summary().await?;
    info!("done: {nstages} stages cracked ({answers} accepted answers total)");
    Ok(())
}

/// Run one generator's candidates through normalize -> dedupe -> md5 ->
/// global lookup -> persist. `seen` spans all phases in a run so a
/// normalized form is only ever hashed once.
async fn phase(
    db: &Db,
    salt: &str,
    targets: &Targets,
    seen: &mut HashSet<u64>,
    method: &str,
    cands: Vec<gen::Candidate>,
) -> Result<()> {
    let t0 = Instant::now();
    let mut hits = Vec::new();
    let mut tried = 0u64;
    for c in cands {
        let norm = normalize_answer(&c.text);
        if norm.is_empty() || !seen.insert(key(&norm)) {
            continue;
        }
        tried += 1;
        let h = answer_hash(salt, &norm);
        if let Some(owners) = targets.get(&h) {
            for (g, idx) in owners {
                hits.push(CrackHit {
                    adventure_guid: g.clone(),
                    stage_index: *idx,
                    hash: h.clone(),
                    plaintext: norm.clone(),
                    display: c.display.clone(),
                    method: method.into(),
                });
            }
        }
    }
    let n = db.save_cracks(hits).await?;
    info!("{method}: {tried} candidates -> {n} new cracks ({:?})", t0.elapsed());
    Ok(())
}

/// Brute-force lengths 1..=len over [a-z0-9]. Resumable per completed
/// length via meta.crack_brute_len.
async fn brute_run(db: &Db, salt: &str, targets: &Targets, max_len: usize) -> Result<()> {
    let done: usize = db
        .get_meta("crack_brute_len")
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for len in 1..=max_len {
        if len <= done {
            info!("brute len {len}: already done, skipping");
            continue;
        }
        let space = 36u64.pow(len as u32);
        if space > 10_000_000_000 {
            warn!("brute len {len}: {space} candidates — this will take a very long time");
        }
        let t0 = Instant::now();
        let hits = gen::brute(salt, targets, len);
        let n = db.save_cracks(hits).await?;
        db.set_meta("crack_brute_len", len.to_string()).await?;
        info!("brute len {len}: {space} candidates -> {n} new cracks ({:?})", t0.elapsed());
    }
    Ok(())
}
