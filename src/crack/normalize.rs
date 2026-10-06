//! Byte-exact port of the app's answer normalization + hashing
//! (com.groundspeak.react.adventures v1.71.0, Hermes bundle):
//!
//!   normalized = input
//!       .replace(/\s/g,   '')   // JS \s — exact set in js_is_space
//!       .replace(/'|'/g,  "'")  // curly -> straight single quotes
//!       .replace(/"|"/g,  '"')  // curly -> straight double quotes
//!       .replace(/—|–/g,  '-')  // em/en dashes -> hyphen
//!       .replace(/Σ/g,    'σ')  // capital -> lowercase sigma
//!       .toLowerCase()
//!   hash = md5(playerPublicGuid + normalized)   // UTF-8 in, lowercase hex
//!
//! Empirically verified: hashing known multiChoiceOptions text reproduces
//! findCodeHashBase16V2 exactly (incl. non-ASCII: 'Gjøvikbanen.', 'Bär').

use std::fmt::Write;

use md5::{Digest, Md5};

/// JavaScript's `\s` character set. Deliberately NOT `char::is_whitespace`:
/// JS also strips U+FEFF (ZWNBSP/BOM) and does NOT strip U+0085 (NEL),
/// which Unicode White_Space includes.
fn js_is_space(c: char) -> bool {
    matches!(c,
        '\t'..='\r' | ' '
        | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
        | '\u{feff}')
}

/// formatAnswerInput() from the app — normalize raw answer text before
/// hashing. Idempotent, so candidates may arrive pre-normalized.
pub fn normalize_answer(input: &str) -> String {
    input
        .chars()
        .filter(|c| !js_is_space(*c))
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2014}' | '\u{2013}' => '-',
            '\u{03a3}' => '\u{03c3}',
            c => c,
        })
        .collect::<String>()
        .to_lowercase()
}

/// md5(publicGuid + normalized) as lowercase hex. The app feeds UTF-8
/// bytes (Paul Johnston md5.js: rstr2hex(rstr_md5(str2rstr_utf8(x)))).
pub fn answer_hash(public_guid: &str, normalized: &str) -> String {
    let mut out = String::with_capacity(32);
    answer_hash_into(public_guid, normalized, &mut out);
    out
}

/// Allocation-free variant for hot loops (brute force): hex into `out`.
pub fn answer_hash_into(public_guid: &str, normalized: &str, out: &mut String) {
    out.clear();
    let mut h = Md5::new();
    h.update(public_guid.as_bytes());
    h.update(normalized.as_bytes());
    for b in h.finalize() {
        let _ = write!(out, "{b:02x}");
    }
}
