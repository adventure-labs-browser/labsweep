//! Candidate generators. Each phase yields answer candidates; the
//! orchestrator normalizes, dedupes, hashes once per unique normalized
//! form, and checks the digest against every stage hash at once (the
//! salt is shared — cost is O(candidates), not O(candidates x stages)).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Result;

use super::normalize::answer_hash_into;
use crate::db::{CorpusRow, CrackHit, TargetStage};

/// Dedupe key for a normalized candidate — 8 bytes instead of a String.
/// A 2^-64 collision just means one candidate is skipped; acceptable.
pub(super) fn key(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// One answer candidate. `display` keeps the pre-normalization text when
/// the source had it (multichoice option, wordlist line).
pub struct Candidate {
    pub text: String,
    pub display: Option<String>,
}

impl From<String> for Candidate {
    fn from(text: String) -> Self {
        Candidate { text, display: None }
    }
}

/// Every option text of every MultiChoice stage — the answer is provably
/// one of them, so this phase doubles as the algorithm's self-test.
pub fn multichoice(stages: &[TargetStage]) -> Vec<Candidate> {
    stages
        .iter()
        .flat_map(|s| {
            s.options.iter().map(|t| Candidate {
                text: t.clone(),
                display: Some(t.clone()),
            })
        })
        .collect()
}

/// Numbers 0..=9999999 plus zero-padded variants ("07", "007", "0042")
/// that don't appear in the plain range.
pub fn numeric() -> Vec<Candidate> {
    let mut v = Vec::with_capacity(11_000_000);
    for n in 0..=9_999_999u32 {
        v.push(n.to_string().into());
    }
    for w in 2..=8usize {
        // only numbers that actually gain a leading zero at this width
        for n in 0..10u64.pow(w as u32 - 1) {
            v.push(format!("{n:0>w$}").into());
        }
    }
    v
}

/// Short structured codes + small curated wordlists:
///   [a-z]{1,3} x [0-9]{1,3} both orders  ("a79", "abc1", "x42")
///   roman numerals 1-500, number words / yes-no / colors across the
///   common lab languages, and a handful of geocaching vocabulary.
pub fn patterns() -> Vec<Candidate> {
    let mut v: Vec<Candidate> = Vec::with_capacity(6_000_000);
    // letters+digits combos, total length <= 5 per side cap (la<=3, di<=3)
    let letters: Vec<String> = (1..=3usize)
        .flat_map(|l| alpha_strings(l))
        .collect();
    let digits: Vec<String> = (1..=3usize)
        .flat_map(|l| digit_strings(l))
        .collect();
    for la in &letters {
        for di in &digits {
            if la.len() + di.len() <= 5 {
                v.push(format!("{la}{di}").into());
                v.push(format!("{di}{la}").into());
            }
        }
    }
    for n in 1..=500 {
        v.push(to_roman(n).into());
    }
    for w in WORDLIST {
        v.push(w.to_string().into());
    }
    v
}

fn alpha_strings(len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let total = 26u64.pow(len as u32);
    for mut n in 0..total {
        let mut s = vec![b'a'; len];
        for i in (0..len).rev() {
            s[i] = b'a' + (n % 26) as u8;
            n /= 26;
        }
        out.push(unsafe { String::from_utf8_unchecked(s) });
    }
    out
}

fn digit_strings(len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let total = 10u64.pow(len as u32);
    for mut n in 0..total {
        let mut s = vec![b'0'; len];
        for i in (0..len).rev() {
            s[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        out.push(unsafe { String::from_utf8_unchecked(s) });
    }
    out
}

fn to_roman(mut n: u32) -> String {
    const T: &[(u32, &str)] = &[
        (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"),
        (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"),
        (4, "iv"), (1, "i"),
    ];
    let mut s = String::new();
    for (v, r) in T {
        while n >= *v {
            s.push_str(r);
            n -= v;
        }
    }
    s
}

/// Number words + yes/no + colors + geo vocab across the common lab
/// languages. Tiny list, high yield for "how many X" answers written out.
const WORDLIST: &[&str] = &[
    // english numbers
    "zero","one","two","three","four","five","six","seven","eight","nine","ten",
    "eleven","twelve","thirteen","fourteen","fifteen","sixteen","seventeen",
    "eighteen","nineteen","twenty","thirty","forty","fifty","sixty","seventy",
    "eighty","ninety","hundred","thousand",
    "first","second","third","fourth","fifth","sixth","seventh","eighth",
    "ninth","tenth",
    // german
    "eins","zwei","drei","vier","funf","fünf","sechs","sieben","acht","neun",
    "zehn","elf","zwolf","zwölf","dreizehn","vierzehn","funfzehn","fünfzehn",
    "sechzehn","siebzehn","achtzehn","neunzehn","zwanzig","dreissig","dreißig",
    "vierzig","funfzig","fünfzig","sechzig","siebzig","achtzig","neunzig",
    "hundert","tausend","erste","zweite","dritte","vierte","funfte","fünfte",
    // french
    "un","une","deux","trois","quatre","cinq","six","sept","huit","neuf","dix",
    "onze","douze","treize","quatorze","quinze","seize","vingt","trente",
    "quarante","cinquante","soixante","cent","mille","premier","premiere",
    "première","deuxieme","deuxième","troisieme","troisième",
    // spanish
    "uno","dos","tres","cuatro","cinco","seis","siete","ocho","nueve","diez",
    "once","doce","trece","catorce","quince","veinte","treinta","cuarenta",
    "cincuenta","sesenta","setenta","ochenta","noventa","cien","ciento","mil",
    "primero","primera","segundo","segunda","tercero","tercera",
    // dutch
    "een","één","twee","drie","vier","vijf","zes","zeven","acht","negen","tien",
    "elf","twaalf","dertien","veertien","vijftien","zestien","zeventien",
    "achttien","negentien","twintig","dertig","veertig","vijftig","zestig",
    "zeventig","tachtig","negentig","honderd","duizend","eerste","tweede",
    "derde","vierde","vijfde",
    // swedish / norwegian-ish
    "ett","en","tva","två","tre","fyra","fem","sex","sju","atta","åtta","nio",
    "tio","elva","tolv","tretton","fjorton","femton","sexton","sjutton",
    "arton","nitton","tjugo","trettio","fyrtio","femtio","sextio","sjuttio",
    "attio","nittio","hundra","tusen","forsta","första","andra","tredje",
    // czech / slovak-ish
    "jeden","jedna","dva","dve","dvě","tri","tři","ctyri","čtyři","pet","pět",
    "sest","šest","sedm","osm","devet","devět","deset","deset","jedenact",
    "jedenáct","dvanact","dvanáct","trinact","třináct","ctrnact","čtrnáct",
    "patnact","patnáct","sestnact","šestnáct","sedmnact","sedmnáct","osmnact",
    "osmnáct","devatenact","devatenáct","dvacet","tricet","třicet","ctyricet",
    "čtyřicet","padesat","padesát","sedesat","šedesát","sedmdesat","sedmdesát",
    "osmdesat","osmdesát","devadesat","devadesát","sto","tisic","tisíc",
    // hungarian
    "egy","ketto","kettő","harom","három","negy","négy","ot","öt","hat",
    "het","hét","nyolc","kilenc","tiz","tíz","tizenegy","tizenkettő","husz",
    "harminc","negyven","otven","ötven","hatvan","hetven","nyolcvan",
    "kilencven","szaz","száz","ezer","elso","első","masodik","második",
    "harmadik","negyedik","otodik","ötödik",
    // polish
    "jeden","jedna","dwa","dwie","trzy","cztery","piec","pięć","szesc","sześć",
    "siedem","osiem","dziewiec","dziewięć","dziesiec","dziesięć","jedenascie",
    "jedenaście","dwanascie","dwanaście","dwadziescia","dwadzieścia","trzydziesci",
    "trzydzieści","czterdziesci","piecdziesiat","pięćdziesiąt","szescdziesiat",
    "sześćdziesiąt","siedemdziesiat","osiemdziesiat","dziewiecdziesiat",
    "dziewięćdziesiąt","sto","tysiac","tysiąc","pierwszy","pierwsza","drugi",
    "druga","trzeci","trzecia",
    // italian
    "uno","due","tre","quattro","cinque","sei","sette","otto","nove","dieci",
    "undici","dodici","tredici","quattordici","quindici","sedici","venti",
    "trenta","quaranta","cinquanta","sessanta","settanta","ottanta","novanta",
    "cento","mille","primo","prima","secondo","seconda","terzo","terza",
    // portuguese
    "um","uma","dois","duas","tres","três","quatro","cinco","seis","sete",
    "oito","nove","dez","onze","doze","treze","catorze","quinze","vinte",
    "trinta","quarenta","cinquenta","sessenta","setenta","oitenta","noventa",
    "cem","cento","mil","primeiro","primeira","segundo","segunda","terceiro",
    "terceira",
    // danish / finnish
    "en","to","tre","fire","fem","seks","syv","otte","ni","ti","elleve","tolv",
    "tretten","fjorten","femten","seksten","sytten","atten","nitten","tyve",
    "tredive","fyrre","halvtreds","tres","halvfjerds","firs","halvfems",
    "yksi","kaksi","kolme","nelja","neljä","viisi","kuusi","seitseman",
    "seitsemän","kahdeksan","yhdeksan","yhdeksän","kymmenen","kaksikymmenta",
    "kaksikymmentä","kolmekymmenta","kolmekymmentä","sata","tuhat","ensimmainen",
    "ensimmäinen",
    // yes / no / affirmations
    "yes","no","ja","nein","oui","non","si","sí","ano","ne","tak","nie","igen",
    "nem","já","sim","nao","não","evet","hayir","hayır","da","net","kylla",
    "kyllä","ei","jaa","jo","nei","waar","false","true","wahr","falsch","vrai",
    "faux","verdadero","vero","prawda","igaz","sant",
    // colors
    "red","blue","green","yellow","black","white","orange","purple","brown",
    "pink","grey","gray","gold","silver","rot","blau","grun","grün","gelb",
    "schwarz","weiss","weiß","braun","rosa","lila","rouge","bleu","vert",
    "verte","jaune","noir","blanc","marron","violet","rood","blauw","groen",
    "geel","zwart","wit","bruin","rood","rod","rød","bla","blå","gron","grøn",
    "gul","svart","hvit","vit","brun","rojo","azul","verde","amarillo","negro",
    "blanco","naranja","morado","rosso","blu","giallo","nero","bianco",
    "vermelho","preto","branco","czerwony","niebieski","zielony","zolty","żółty",
    "czarny","bialy","biały","cervena","červená","modra","modrá","zelena",
    "zelená","zluta","žlutá","cerna","černá","bila","bílá","piros","kek","kék",
    "zold","zöld","sarga","sárga","fekete","feher","fehér",
    // geocaching vocab + common answer words
    "geocache","geocaching","adventure","adventurelab","lab","cache","bonus",
    "final","answer","antwort","odpowiedz","odpověď","svar","valasz","válasz",
    "reponse","réponse","respuesta","risposta","resposta","antwoord","code",
    "password","passwort","heslo","jelszo","jelszó","haslo","hasło","key",
    "schlussel","schlüssel","cle","clé","llave","chiave","klucz","klíč","kulcs",
    "found","foundit","ftf","congratulations","gratulere","gratuliere","done",
    "fertig","fin","fim","klaar","klart","hotovo","gotowe","kész","fertig",
];

/// Corpus single-words mangled with the suffixes creators actually use —
/// word+digits ("psalm90", "a79"), plurals, punctuation. Chunked like
/// corpus(); dedupe per chunk, cross-chunk in the orchestrator.
pub fn mangle(rows: &[CorpusRow]) -> Vec<Candidate> {
    let mut seen: HashSet<u64> = HashSet::new();
    let mut words: Vec<String> = Vec::new();
    let collect = |f: &str, seen: &mut HashSet<u64>, words: &mut Vec<String>| {
        for w in f
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty() && w.len() <= 12)
        {
            // need at least one letter — pure digits are numeric()'s job
            if !w.chars().any(|c| c.is_alphabetic()) {
                continue;
            }
            let norm = super::normalize_answer(w);
            if !norm.is_empty() && seen.insert(key(&norm)) {
                words.push(norm);
            }
        }
    };
    for r in rows {
        for f in [
            &r.title,
            &r.question,
            &r.description,
            &r.adv_title,
            &r.adv_description,
        ] {
            collect(f, &mut seen, &mut words);
        }
        for o in &r.options {
            collect(o, &mut seen, &mut words);
        }
    }
    let mut out = Vec::with_capacity(words.len() * 35);
    for w in &words {
        for n in 0..=20u32 {
            out.push(format!("{w}{n}").into());
            if n <= 9 {
                out.push(format!("{n}{w}").into());
            }
        }
        for suf in ["s", "'s", "!", "123"] {
            out.push(format!("{w}{suf}").into());
        }
    }
    out
}

/// Words and n-grams (up to 4) mined from stage + adventure text plus
/// multichoice options — "the word on the sign" is usually right there.
/// Called per corpus_rows() chunk; dedupes within the chunk on the
/// normalized form (u64 keys, bounded memory). Cross-chunk dedupe
/// happens in the orchestrator's `seen` set.
pub fn corpus(rows: &[CorpusRow]) -> Vec<Candidate> {
    let mut seen: HashSet<u64> = HashSet::new();
    let mut out: Vec<Candidate> = Vec::new();
    let feed = |f: &str, seen: &mut HashSet<u64>, out: &mut Vec<Candidate>| {
        let mut emit = |norm: String| {
            if !norm.is_empty() && norm.len() <= 64 && seen.insert(key(&norm)) {
                out.push(Candidate {
                    text: norm,
                    display: None,
                });
            }
        };
        let words: Vec<&str> = f
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        for n in 1..=4usize {
            if words.len() < n {
                break;
            }
            for w in words.windows(n) {
                emit(super::normalize_answer(&w.join(" ")));
            }
        }
        // the whole field, when short enough to plausibly be an answer
        if f.len() <= 80 {
            emit(super::normalize_answer(f));
        }
    };
    for r in rows {
        for f in [
            &r.title,
            &r.question,
            &r.description,
            &r.adv_title,
            &r.adv_description,
        ] {
            feed(f, &mut seen, &mut out);
        }
        for o in &r.options {
            feed(o, &mut seen, &mut out);
        }
    }
    out
}

/// One candidate per line from a wordlist file.
pub fn wordlist(path: &Path) -> Result<Vec<Candidate>> {
    let txt = std::fs::read_to_string(path)?;
    Ok(txt
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| Candidate {
            text: l.to_string(),
            display: Some(l.to_string()),
        })
        .collect())
}

const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Every [a-z0-9] string of exactly `len`, split across threads by
/// counter range. Candidates are normalized by construction. Returns the
/// hits; the caller batches them into the db.
pub fn brute(
    salt: &str,
    targets: &super::Targets,
    len: usize,
) -> Vec<CrackHit> {
    let total = (CHARSET.len() as u64).pow(len as u32);
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(32);
    let chunk = total.div_ceil(nthreads as u64);
    let mut hits = Vec::new();
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for t in 0..nthreads {
            let lo = t as u64 * chunk;
            let hi = (lo + chunk).min(total);
            if lo >= hi {
                break;
            }
            handles.push(s.spawn(move || {
                let mut local = Vec::new();
                let mut buf = vec![0u8; len];
                let mut hex = String::with_capacity(32);
                for n in lo..hi {
                    let mut m = n;
                    for i in (0..len).rev() {
                        buf[i] = CHARSET[(m % 36) as usize];
                        m /= 36;
                    }
                    // charset is ASCII — safe
                    let cand = unsafe { std::str::from_utf8_unchecked(&buf) };
                    answer_hash_into(salt, cand, &mut hex);
                    if let Some(owners) = targets.get(hex.as_str()) {
                        for (g, idx) in owners {
                            local.push(CrackHit {
                                adventure_guid: g.clone(),
                                stage_index: *idx,
                                hash: hex.clone(),
                                plaintext: cand.to_string(),
                                display: None,
                                method: "brute".into(),
                            });
                        }
                    }
                }
                local
            }));
        }
        for h in handles {
            hits.extend(h.join().unwrap());
        }
    });
    hits
}
