//! Solr analyzer chains for the text fields.
//!
//! Three chains are covered, taken from `managed-schema.xml`:
//!
//! * `text_en_splitting`: whitespace, `icuFolding`, `wordDelimiterGraph`, lowercase,
//!   keyword marker, Porter stemmer.
//! * `text_international`: standard tokenizer, `icuFolding` (the stop list is empty).
//! * `text_general`: standard tokenizer, lowercase (the stop list is empty).
//!
//! Field types with any other analyzer use [`Chain::WhitespaceLowercase`] as a fallback
//! (see `schema::AnalyzerSpec::chain`).
//!
//! Query-time synonym expansion is not implemented. `icuFolding` is approximated by
//! NFKD, dropping combining marks, case folding and a small extra map, like the Python code.

use std::borrow::Cow;

use crate::porter;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Which analyzer chain a text field uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chain {
    /// Solr `text_en_splitting`.
    EnSplitting,
    /// Solr `text_international`.
    International,
    /// Solr `text_general`.
    General,
    /// Fallback for field types whose analyzer is not implemented: whitespace, lowercase.
    WhitespaceLowercase,
}

/// The tokens of one index-time analysis.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Analysis {
    /// The first token of every position.
    pub main: Vec<String>,
    /// Extra tokens stacked on a position (catenated word and number parts), without duplicates.
    pub extra: Vec<String>,
}

/// A token position: the main token plus tokens stacked on it.
struct Pos {
    main: String,
    stacked: Vec<String>,
}

impl Pos {
    fn single(main: String) -> Self {
        Self {
            main,
            stacked: Vec::new(),
        }
    }

    fn map(self, f: impl Fn(&str) -> String) -> Self {
        Self {
            main: f(&self.main),
            stacked: self.stacked.iter().map(|t| f(t)).collect(),
        }
    }
}

/// Words the keyword marker protects from stemming (`protwords.txt`).
const PROTECTED: &[&str] = &["dontstems", "zwhacky"];

/// Runs analyzer chains.
#[derive(Clone, Copy, Debug, Default)]
pub struct Analyzer;

impl Analyzer {
    /// Builds an analyzer.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Analyzes `text` the way the index does.
    #[must_use]
    pub fn index(self, chain: Chain, text: &str) -> Analysis {
        let mut out = Analysis::default();
        for pos in Self::run(chain, text, true) {
            for t in pos.stacked {
                if t != pos.main && !out.extra.contains(&t) {
                    out.extra.push(t);
                }
            }
            out.main.push(pos.main);
        }
        out
    }

    /// Analyzes `text` the way a query does: one token per position.
    #[must_use]
    pub fn query(self, chain: Chain, text: &str) -> Vec<String> {
        Self::run(chain, text, false)
            .into_iter()
            .map(|p| p.main)
            .collect()
    }

    fn run(chain: Chain, text: &str, index_time: bool) -> Vec<Pos> {
        match chain {
            Chain::EnSplitting => text
                .split_whitespace()
                .flat_map(|w| word_delimiter(&fold(w), index_time))
                .map(|p| p.map(Self::finish_en))
                .collect(),
            Chain::International => standard_tokenize(text)
                .into_iter()
                .map(|t| Pos::single(fold(t)))
                .collect(),
            Chain::General => standard_tokenize(text)
                .into_iter()
                .map(|t| Pos::single(t.to_lowercase()))
                .collect(),
            Chain::WhitespaceLowercase => text
                .split_whitespace()
                .map(|t| Pos::single(t.to_lowercase()))
                .collect(),
        }
    }

    fn finish_en(token: &str) -> String {
        let lower = token.to_lowercase();
        if PROTECTED.contains(&lower.as_str()) {
            lower
        } else {
            porter::stem(&lower)
        }
    }
}

/// Approximates ICU folding: NFKD, strip combining marks, case fold, a few extra letters, NFC.
#[must_use]
pub fn fold(s: &str) -> String {
    let mut folded = String::with_capacity(s.len());
    for c in s.nfkd().filter(|c| !is_combining_mark(*c)) {
        folded.push_str(&case_fold(c));
    }
    folded.nfc().collect()
}

fn case_fold(c: char) -> Cow<'static, str> {
    match c {
        'ß' => Cow::Borrowed("ss"),
        'æ' | 'Æ' => Cow::Borrowed("ae"),
        'œ' | 'Œ' => Cow::Borrowed("oe"),
        'ø' | 'Ø' => Cow::Borrowed("o"),
        'đ' | 'Đ' | 'ð' | 'Ð' => Cow::Borrowed("d"),
        'ł' | 'Ł' => Cow::Borrowed("l"),
        'þ' | 'Þ' => Cow::Borrowed("th"),
        'ı' => Cow::Borrowed("i"),
        'ħ' | 'Ħ' => Cow::Borrowed("h"),
        'ς' => Cow::Borrowed("σ"),
        'ſ' => Cow::Borrowed("s"),
        _ => Cow::Owned(c.to_lowercase().collect()),
    }
}

fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric() || is_combining_mark(c)
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '\u{AC00}'..='\u{D7AF}')
}

fn is_latin_word_char(c: char) -> bool {
    is_word_char(c) && !is_cjk(c)
}

/// A simplified UAX #29 word tokenizer: runs of word characters, `'` `’` `.` joining two
/// word characters, and one token per CJK character.
#[must_use]
pub fn standard_tokenize(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if !is_word_char(c) {
            continue;
        }
        let mut end = start + c.len_utf8();
        if !is_cjk(c) {
            while let Some((i, d)) = chars.peek().copied() {
                if is_latin_word_char(d) {
                    end = i + d.len_utf8();
                    chars.next();
                } else if matches!(d, '\'' | '\u{2019}' | '.') {
                    let mut ahead = chars.clone();
                    ahead.next();
                    match ahead.peek().copied() {
                        Some((k, e)) if is_latin_word_char(e) => {
                            end = k + e.len_utf8();
                            chars.next();
                            chars.next();
                        }
                        _ => break,
                    }
                } else {
                    break;
                }
            }
        }
        if let Some(tok) = text.get(start..end) {
            out.push(tok);
        }
    }
    out
}

const LOWER: u8 = 1;
const UPPER: u8 = 2;
const DIGIT: u8 = 4;
const ALPHA: u8 = LOWER | UPPER;
const ALPHANUM: u8 = ALPHA | DIGIT;
const DELIM: u8 = 8;

/// Character classes, with the two overrides from `wordtypes.txt`.
fn char_type(c: char) -> u8 {
    match c {
        '\u{A9}' => ALPHANUM,
        '#' => ALPHA,
        _ if c.is_uppercase() => UPPER,
        _ if c.is_lowercase() => LOWER,
        _ if c.is_alphabetic() || is_combining_mark(c) => ALPHA,
        _ if c.is_numeric() => DIGIT,
        _ => DELIM,
    }
}

fn is_break(last: u8, cur: u8) -> bool {
    last & cur == 0 && !(last == UPPER && cur & ALPHA != 0)
}

/// Splits a word at delimiters, letter/digit changes and lower-to-upper case changes.
fn split_parts(word: &str) -> Vec<(&str, u8)> {
    let mut parts: Vec<(&str, u8)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut last = 0;
    for (i, c) in word.char_indices() {
        let t = char_type(c);
        if t == DELIM {
            if let Some(s) = start.take()
                && let Some(p) = word.get(s..i)
            {
                parts.push((p, last));
            }
            last = 0;
            continue;
        }
        match start {
            None => start = Some(i),
            Some(s) if is_break(last, t) => {
                if let Some(p) = word.get(s..i) {
                    parts.push((p, last));
                }
                start = Some(i);
            }
            Some(_) => {}
        }
        last = t;
    }
    if let Some(s) = start
        && let Some(p) = word.get(s..)
    {
        parts.push((p, last));
    }
    let possessive = word
        .chars()
        .rev()
        .nth(1)
        .is_some_and(|c| c == '\'' || c == '\u{2019}');
    if possessive && parts.last().is_some_and(|(p, _)| matches!(*p, "s" | "S")) {
        parts.pop();
    }
    parts
}

/// `wordDelimiterGraph` with `generateWordParts`, `generateNumberParts` and
/// `splitOnCaseChange` on; at index time words and numbers are also catenated.
fn word_delimiter(word: &str, index_time: bool) -> Vec<Pos> {
    let parts = split_parts(word);
    let mut stacked = Vec::new();
    if index_time {
        let words: Vec<&str> = parts
            .iter()
            .filter(|(_, t)| *t != DIGIT)
            .map(|(p, _)| *p)
            .collect();
        let nums: Vec<&str> = parts
            .iter()
            .filter(|(_, t)| *t == DIGIT)
            .map(|(p, _)| *p)
            .collect();
        if words.len() > 1 {
            stacked.push(words.concat());
        }
        if nums.len() > 1 {
            stacked.push(nums.concat());
        }
    }
    let mut out: Vec<Pos> = parts
        .into_iter()
        .map(|(p, _)| Pos::single(p.to_owned()))
        .collect();
    if let Some(first) = out.first_mut() {
        first.stacked = stacked;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_accents_and_ligatures() {
        assert_eq!(fold("Åström Straße"), "astrom strasse");
        assert_eq!(fold("Æsop"), "aesop");
    }

    #[test]
    fn standard_tokenizer_joins_apostrophes_and_dots() {
        assert_eq!(
            standard_tokenize("Gulliver's Travels, U.S.A."),
            ["Gulliver's", "Travels", "U.S.A"]
        );
        assert_eq!(standard_tokenize("日本語"), ["日", "本", "語"]);
    }

    #[test]
    fn word_delimiter_splits_and_catenates() {
        let a = Analyzer::new();
        let got = a.index(Chain::EnSplitting, "Wi-Fi 802.11b");
        assert_eq!(got.main, ["wi", "fi", "802", "11", "b"]);
        assert_eq!(got.extra, ["wifi", "80211"]);
        assert_eq!(a.query(Chain::EnSplitting, "Wi-Fi"), ["wi", "fi"]);
        assert_eq!(a.query(Chain::EnSplitting, "PowerShot"), ["powershot"]);
    }

    #[test]
    fn possessive_and_protected_words() {
        let a = Analyzer::new();
        assert_eq!(a.query(Chain::EnSplitting, "o'neil's"), ["o", "neil"]);
        assert_eq!(a.query(Chain::EnSplitting, "dontstems"), ["dontstems"]);
    }

    #[test]
    fn international_chain_folds_without_stemming() {
        let a = Analyzer::new();
        assert_eq!(
            a.index(Chain::International, "Gabriel García Márquez").main,
            ["gabriel", "garcia", "marquez"]
        );
    }
}
