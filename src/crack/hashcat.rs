//! Interop with hashcat for attacks beyond what the built-in generators
//! reach (rule-mangled dictionaries, long brute force on faster CPUs).
//!
//! The scheme md5(publicGuid + answer) is hashcat mode 20
//! (`md5($salt.$pass)`) with the guid as salt:
//!
//!   labsweep crack --export-hashes targets.txt   # hash:salt per line
//!   hashcat -m 20 targets.txt wordlist.txt -r rules/best64.rule
//!   labsweep crack --import-hashes hashcat.potfile

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::Result;

use super::Targets;
use crate::db::CrackHit;

/// Write every unique target hash as a `hash:salt` line — ready for
/// `hashcat -m 20`.
pub fn export(path: &Path, salt: &str, targets: &Targets) -> Result<usize> {
    let mut f = BufWriter::new(File::create(path)?);
    let mut n = 0usize;
    for h in targets.keys() {
        writeln!(f, "{h}:{salt}")?;
        n += 1;
    }
    Ok(n)
}

/// Read a hashcat potfile (`hash:plain` lines) and map cracked hashes
/// back to their stages. `plain` is the normalized candidate hashcat
/// guessed — un-normalizing is impossible by design.
pub fn import(path: &Path, targets: &Targets) -> Result<Vec<CrackHit>> {
    let txt = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in txt.lines() {
        let Some((h, plain)) = line.split_once(':') else {
            continue;
        };
        if let Some(owners) = targets.get(h) {
            for (g, idx) in owners {
                out.push(CrackHit {
                    adventure_guid: g.clone(),
                    stage_index: *idx,
                    hash: h.to_string(),
                    plaintext: plain.to_string(),
                    display: None,
                    method: "hashcat".into(),
                });
            }
        }
    }
    Ok(out)
}
