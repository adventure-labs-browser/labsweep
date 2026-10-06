//! Pre-flight proof that salt + normalization are byte-exact, before any
//! compute is spent guessing. MultiChoice stages ship their options, and
//! the answer is provably one of them: if hashing options reproduces a
//! stage's stored hashes, the whole pipeline is right. Zero matches on a
//! healthy sample means the salt or normalization is wrong — abort.

use anyhow::{bail, Result};
use tracing::{info, warn};

use super::normalize::{answer_hash, normalize_answer};
use crate::db::TargetStage;

pub fn self_test(salt: &str, stages: &[TargetStage]) -> Result<()> {
    let mut total = 0usize;
    let mut matched = 0usize;
    for s in stages
        .iter()
        .filter(|s| !s.options.is_empty() && !s.hashes.is_empty())
        .take(2000)
    {
        total += 1;
        let hit = s.options.iter().any(|o| {
            let h = answer_hash(salt, &normalize_answer(o));
            s.hashes.iter().any(|x| x == &h)
        });
        if hit {
            matched += 1;
        }
    }
    if total == 0 {
        warn!("self-test: no MultiChoice stages to check against — skipping");
        return Ok(());
    }
    info!("self-test: {matched}/{total} multichoice stages verify");
    if matched == 0 {
        bail!("self-test FAILED — salt or normalization is wrong, aborting");
    }
    Ok(())
}
