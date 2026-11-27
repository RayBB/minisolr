//! Solr analyzer chains, built from the `<analyzer>` elements of `managed-schema.xml` the
//! way Python minisolr's `analysis.py` builds them.
//!
//! A chain is a tokenizer and a list of filters. Positions are lists of alternative
//! tokens, the first being the main token (synonyms and catenated word parts stack on a
//! position). Tokenizers: `whitespace`, `keyword`, and the standard tokenizer for every
//! other name (as Python, which has no Japanese, Korean, Thai or path tokenizers).
//! Filters: `lowercase`, `stop`, `icuFolding`, `wordDelimiterGraph`, `porterStem`,
//! `snowballPorter` (Snowball English for every language, as Python), `englishPossessive`, `englishMinimalStem`, `keywordMarker`, `synonymGraph` and
//! `removeDuplicates`; any other filter is skipped and reported by [`Analyzer::build`]
//! (Python skips them silently). Word lists come from the files the filters name, relative
//! to the conf directory ([`Resources`]); a missing file is an empty list, as in Python.
//!
//! `icuFolding` is approximated as in Python: NFKD, drop nonspacing marks (`Mn` only),
//! case fold, a few extra letters, NFC. Character categories come from [`crate::unicode`],
//! generated from Python's `unicodedata`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::unicode::{self, Class};
use crate::{porter, snowball};

/// One token, with the keyword flag `keywordMarker` sets (stemmers leave keywords alone).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tok {
    text: String,
    kw: bool,
}

impl Tok {
    fn new(text: impl Into<String>, kw: bool) -> Self {
        Self {
            text: text.into(),
            kw,
        }
    }
}

/// Positions of alternative tokens.
type Stream = Vec<Vec<Tok>>;

/// The tokens of one index-time analysis.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Analysis {
    /// The first token of every position.
    pub main: Vec<String>,
    /// Other tokens stacked on a position (catenated word parts, synonyms), each once.
    pub extra: Vec<String>,
}

/// Word list files next to the schema (`stopwords.txt`, `protwords.txt`, ...).
#[derive(Clone, Debug, Default)]
pub struct Resources {
    dir: Option<PathBuf>,
}

impl Resources {
    /// Files are read from `dir`; with `None` every list is empty.
    #[must_use]
    pub fn new(dir: Option<&Path>) -> Self {
        Self {
            dir: dir.map(Path::to_path_buf),
        }
    }

    /// The stripped, non-empty lines of `name` that do not start with `#`, as Python's
    /// `Resources.lines`; empty when the file does not exist.
    #[must_use]
    pub fn lines(&self, name: &str) -> Vec<String> {
        let Some(path) = self.dir.as_ref().map(|d| d.join(name)) else {
            return Vec::new();
        };
        if !path.is_file() {
            return Vec::new();
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .map(|l| l.trim_matches(unicode::is_space))
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned)
                .collect(),
            Err(e) => {
                eprintln!("warning: cannot read {}: {e}", path.display());
                Vec::new()
            }
        }
    }
}

/// A `<filter>` or `<tokenizer>` element: its lowercased SPI name and its attributes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Component {
    /// Lowercased SPI name, such as `worddelimitergraph`.
    pub name: String,
    /// The element's attributes as written (names are case-sensitive, as in Python).
    pub attrs: HashMap<String, String>,
}

impl Component {
    /// Python's `_flag`: the attribute (or `default`) is true unless it is `0` or `false`.
    fn flag(&self, key: &str, default: bool) -> bool {
        self.attrs.get(key).map_or(default, |v| {
            !matches!(v.trim_matches(unicode::is_space), "0" | "false")
        })
    }

    fn attr<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.attrs.get(key).map_or(default, String::as_str)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tokenizer {
    /// Python's `str.split()`.
    Whitespace,
    /// The whole text as one token (none when empty).
    Keyword,
    /// The simplified UAX #29 tokenizer ([`standard_tokenize`]).
    Standard,
    /// The whole text as one token even when empty: a text type without an analyzer.
    Raw,
}

#[derive(Clone, Debug)]
enum Filter {
    Lowercase,
    Fold,
    Stop {
        ignore_case: bool,
        words: HashSet<String>,
    },
    Possessive,
    KeywordMarker {
        ignore_case: bool,
        words: HashSet<String>,
    },
    Porter,
    Snowball,
    MinimalStem,
    RemoveDuplicates,
    Synonyms(Synonyms),
    WordDelimiter(WordDelimiter),
}

/// An analyzer chain.
#[derive(Clone, Debug)]
pub struct Analyzer {
    tokenizer: Tokenizer,
    filters: Vec<Filter>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::raw()
    }
}

impl Analyzer {
    /// The analysis of a text type without an analyzer: the value is one token, as Python's
    /// `[str(x)]` / `[[text]]`.
    #[must_use]
    pub fn raw() -> Self {
        Self {
            tokenizer: Tokenizer::Raw,
            filters: Vec::new(),
        }
    }

    /// Builds the chain of a tokenizer and filters. Returns the analyzer and the names of
    /// the components it does not implement: filters, which are skipped, and tokenizers,
    /// for which the standard tokenizer is used (both as Python does).
    #[must_use]
    pub fn build(
        tokenizer: &Component,
        filters: &[Component],
        res: &Resources,
    ) -> (Self, Vec<String>) {
        let mut skipped = Vec::new();
        let tokenizer = match tokenizer.name.as_str() {
            "whitespace" => Tokenizer::Whitespace,
            "keyword" => Tokenizer::Keyword,
            "standard" | "" => Tokenizer::Standard,
            other => {
                skipped.push(format!("tokenizer {other} (standard used)"));
                Tokenizer::Standard
            }
        };
        let mut out = Vec::new();
        for f in filters {
            let filter = match f.name.as_str() {
                "lowercase" => Filter::Lowercase,
                "icufolding" => Filter::Fold,
                "stop" => {
                    let ignore_case = f.flag("ignoreCase", false);
                    Filter::Stop {
                        ignore_case,
                        words: word_set(res, f.attr("words", "stopwords.txt"), ignore_case),
                    }
                }
                "englishpossessive" => Filter::Possessive,
                "keywordmarker" => {
                    let ignore_case = f.flag("ignoreCase", false);
                    Filter::KeywordMarker {
                        ignore_case,
                        words: word_set(res, f.attr("protected", "protwords.txt"), ignore_case),
                    }
                }
                "porterstem" => Filter::Porter,
                // Python stems with Snowball English whatever the `language` attribute says.
                "snowballporter" => Filter::Snowball,
                "englishminimalstem" => Filter::MinimalStem,
                "removeduplicates" => Filter::RemoveDuplicates,
                "synonymgraph" => Filter::Synonyms(Synonyms::new(f, res)),
                "worddelimitergraph" => Filter::WordDelimiter(WordDelimiter::new(f, res)),
                // Graph flattening does not change token text or positions here.
                "flattengraph" => continue,
                other => {
                    skipped.push(other.to_owned());
                    continue;
                }
            };
            out.push(filter);
        }
        (
            Self {
                tokenizer,
                filters: out,
            },
            skipped,
        )
    }

    /// Analyzes `text` the way the index does (Python's `AnalyzerSpec.index`).
    #[must_use]
    pub fn index(&self, text: &str) -> Analysis {
        let mut out = Analysis::default();
        for pos in self.run(text) {
            let mut alts = pos.into_iter();
            let Some(main) = alts.next() else {
                continue;
            };
            for t in alts {
                if t.text != main.text && !out.extra.contains(&t.text) {
                    out.extra.push(t.text);
                }
            }
            out.main.push(main.text);
        }
        out
    }

    /// Analyzes `text` the way a query does: the distinct alternatives of each position
    /// (Python's `AnalyzerSpec.query`).
    #[must_use]
    pub fn query(&self, text: &str) -> Vec<Vec<String>> {
        self.run(text)
            .into_iter()
            .filter_map(|pos| {
                let mut alts: Vec<String> = Vec::with_capacity(pos.len());
                for t in pos {
                    if !alts.contains(&t.text) {
                        alts.push(t.text);
                    }
                }
                (!alts.is_empty()).then_some(alts)
            })
            .collect()
    }

    fn run(&self, text: &str) -> Stream {
        let tokens: Vec<&str> = match self.tokenizer {
            Tokenizer::Whitespace => unicode::split_whitespace(text).collect(),
            Tokenizer::Keyword if text.is_empty() => Vec::new(),
            Tokenizer::Keyword | Tokenizer::Raw => vec![text],
            Tokenizer::Standard => standard_tokenize(text),
        };
        let mut stream: Stream = tokens
            .into_iter()
            .map(|t| vec![Tok::new(t, false)])
            .collect();
        for f in &self.filters {
            stream = f.apply(stream);
        }
        stream
    }
}

fn word_set(res: &Resources, file: &str, ignore_case: bool) -> HashSet<String> {
    res.lines(file)
        .into_iter()
        .map(|w| if ignore_case { w.to_lowercase() } else { w })
        .collect()
}

/// Python's `_per_token`: maps every token, dropping `None`s and emptied positions.
fn per_token(stream: Stream, f: impl Fn(Tok) -> Option<Tok>) -> Stream {
    stream
        .into_iter()
        .filter_map(|pos| {
            let new: Vec<Tok> = pos.into_iter().filter_map(&f).collect();
            (!new.is_empty()).then_some(new)
        })
        .collect()
}

fn matches_word(words: &HashSet<String>, text: &str, ignore_case: bool) -> bool {
    if ignore_case {
        words.contains(&text.to_lowercase())
    } else {
        words.contains(text)
    }
}

impl Filter {
    fn apply(&self, stream: Stream) -> Stream {
        match self {
            Self::Lowercase => per_token(stream, |t| Some(Tok::new(t.text.to_lowercase(), t.kw))),
            Self::Fold => per_token(stream, |t| Some(Tok::new(fold(&t.text), t.kw))),
            Self::Stop { ignore_case, words } => per_token(stream, |t| {
                (!matches_word(words, &t.text, *ignore_case)).then_some(t)
            }),
            Self::Possessive => per_token(stream, |t| {
                let chars: Vec<char> = t.text.chars().collect();
                match chars.as_slice() {
                    [rest @ .., '\'' | '\u{2019}', 's' | 'S'] => {
                        Some(Tok::new(rest.iter().collect::<String>(), t.kw))
                    }
                    _ => Some(t),
                }
            }),
            Self::KeywordMarker { ignore_case, words } => per_token(stream, |t| {
                if matches_word(words, &t.text, *ignore_case) {
                    Some(Tok::new(t.text, true))
                } else {
                    Some(t)
                }
            }),
            Self::Porter => per_token(stream, |t| {
                Some(if t.kw {
                    t
                } else {
                    Tok::new(porter::stem(&t.text), false)
                })
            }),
            Self::Snowball => per_token(stream, |t| {
                Some(if t.kw {
                    t
                } else {
                    Tok::new(snowball::stem(&t.text), false)
                })
            }),
            Self::MinimalStem => per_token(stream, |t| {
                Some(if t.kw {
                    t
                } else {
                    Tok::new(minimal_stem(&t.text), false)
                })
            }),
            Self::RemoveDuplicates => stream
                .into_iter()
                .map(|pos| {
                    let mut seen = HashSet::new();
                    pos.into_iter()
                        .filter(|t| seen.insert(t.text.clone()))
                        .collect()
                })
                .collect(),
            Self::Synonyms(s) => stream
                .into_iter()
                .map(|pos| pos.into_iter().flat_map(|t| s.expand(t)).collect())
                .collect(),
            Self::WordDelimiter(w) => w.apply(stream),
        }
    }
}

/// `englishMinimalStem`, as Python's `_minimal_stem`.
fn minimal_stem(w: &str) -> String {
    let c: Vec<char> = w.chars().collect();
    let n = c.len();
    let from_end = |k: usize| n.checked_sub(k).and_then(|i| c.get(i)).copied();
    if n < 3 || from_end(1) != Some('s') {
        return w.to_owned();
    }
    match from_end(2) {
        Some('u' | 's') => return w.to_owned(),
        Some('e') => {
            if n > 3 && from_end(3) == Some('i') && !matches!(from_end(4), Some('a' | 'e')) {
                return c.iter().take(n - 3).chain(['y'].iter()).collect();
            }
            if matches!(from_end(3), Some('i' | 'a' | 'o' | 'e')) {
                return w.to_owned();
            }
        }
        _ => {}
    }
    c.iter().take(n - 1).collect()
}

/// `synonymGraph` with Python's approximation: single-word rules only (a rule with a
/// multi-word term on either side is ignored), explicit `a, b => c, d` mappings replace
/// the token, equivalence groups `a, b, c` stack the other terms on the token's position
/// (`expand=true`) or replace it with the group's first term (`expand=false`).
#[derive(Clone, Debug, Default)]
struct Synonyms {
    ignore_case: bool,
    expand: bool,
    explicit: HashMap<String, Vec<String>>,
    groups: HashMap<String, Vec<String>>,
}

impl Synonyms {
    fn new(f: &Component, res: &Resources) -> Self {
        let mut s = Self {
            ignore_case: f.flag("ignoreCase", false),
            expand: f.flag("expand", true),
            ..Self::default()
        };
        let terms = |side: &str, s: &Self| -> Vec<String> {
            side.split(',')
                .map(|x| x.trim_matches(unicode::is_space))
                .filter(|x| !x.is_empty())
                .map(|x| s.norm(x))
                .collect()
        };
        for line in res.lines(f.attr("synonyms", "synonyms.txt")) {
            if let Some((lhs, rhs)) = line.split_once("=>") {
                let ins = terms(lhs, &s);
                let outs = terms(rhs, &s);
                if ins.iter().chain(&outs).any(|x| x.contains(' ')) {
                    continue;
                }
                for i in ins {
                    s.explicit.insert(i, outs.clone());
                }
            } else {
                let group = terms(&line, &s);
                if group.is_empty() || group.iter().any(|x| x.contains(' ')) {
                    continue;
                }
                for t in &group {
                    s.groups.insert(t.clone(), group.clone());
                }
            }
        }
        s
    }

    fn norm(&self, s: &str) -> String {
        if self.ignore_case {
            s.to_lowercase()
        } else {
            s.to_owned()
        }
    }

    fn expand(&self, t: Tok) -> Vec<Tok> {
        let key = self.norm(&t.text);
        if let Some(outs) = self.explicit.get(&key) {
            return outs.iter().map(|o| Tok::new(o.clone(), t.kw)).collect();
        }
        if let Some(group) = self.groups.get(&key) {
            if !self.expand {
                return group
                    .first()
                    .map(|g| Tok::new(g.clone(), t.kw))
                    .into_iter()
                    .collect();
            }
            let kw = t.kw;
            let mut out = vec![t];
            out.extend(
                group
                    .iter()
                    .filter(|o| **o != key)
                    .map(|o| Tok::new(o.clone(), kw)),
            );
            return out;
        }
        vec![t]
    }
}

/// Approximates ICU folding as Python's `fold`: NFKD, drop nonspacing marks, case fold,
/// a few extra letters, NFC.
#[must_use]
pub fn fold(s: &str) -> String {
    let mut folded = String::with_capacity(s.len());
    for c in s.nfkd() {
        if unicode::class(c) != Class::Nonspacing {
            unicode::push_casefold(c, &mut folded);
        }
    }
    let mut mapped = String::with_capacity(folded.len());
    for c in folded.chars() {
        match c {
            'ß' => mapped.push_str("ss"),
            'æ' => mapped.push_str("ae"),
            'œ' => mapped.push_str("oe"),
            'ø' => mapped.push('o'),
            'đ' | 'ð' => mapped.push('d'),
            'ł' => mapped.push('l'),
            'þ' => mapped.push_str("th"),
            'ı' => mapped.push('i'),
            'ħ' => mapped.push('h'),
            other => mapped.push(other),
        }
    }
    mapped.nfc().collect()
}

fn is_word_char(c: char) -> bool {
    c == '_' || unicode::is_letter(c) || unicode::is_number(c) || unicode::is_mark(c)
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '\u{AC00}'..='\u{D7AF}')
}

fn is_latin_word_char(c: char) -> bool {
    is_word_char(c) && !is_cjk(c)
}

/// A simplified UAX #29 word tokenizer (Python's `standard_tokenize`): runs of word
/// characters, `'` `’` `.` joining two word characters, and one token per CJK character.
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
const DELIM: u8 = 8;
const ALPHA: u8 = LOWER | UPPER;
const ALPHANUM: u8 = ALPHA | DIGIT;

/// `wordDelimiterGraph` as Python's `_make_wdgf`: `generateWordParts`,
/// `generateNumberParts`, `catenateWords`, `catenateNumbers` and a `types` file are read;
/// words always split on case changes and letter/digit changes, and a trailing `'s` is
/// always dropped. A word's parts take successive positions; catenated parts stack on the
/// first one.
// The four flags are the filter's independent attributes.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default)]
struct WordDelimiter {
    gen_words: bool,
    gen_nums: bool,
    cat_words: bool,
    cat_nums: bool,
    overrides: HashMap<char, u8>,
}

impl WordDelimiter {
    fn new(f: &Component, res: &Resources) -> Self {
        let mut overrides = HashMap::new();
        if let Some(file) = f.attrs.get("types") {
            for line in res.lines(file) {
                let Some((lhs, rhs)) = line.split_once("=>") else {
                    continue;
                };
                let value = match rhs.trim_matches(unicode::is_space) {
                    "LOWER" => LOWER,
                    "UPPER" => UPPER,
                    "DIGIT" => DIGIT,
                    "ALPHA" => ALPHA,
                    "ALPHANUM" => ALPHANUM,
                    "SUBWORD_DELIM" => DELIM,
                    _ => continue,
                };
                let key = unescape(lhs.trim_matches(unicode::is_space));
                let mut chars = key.chars();
                // Python keys the map by the whole string; only one character can match.
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    overrides.insert(c, value);
                }
            }
        }
        Self {
            gen_words: f.flag("generateWordParts", true),
            gen_nums: f.flag("generateNumberParts", true),
            cat_words: f.flag("catenateWords", false),
            cat_nums: f.flag("catenateNumbers", false),
            overrides,
        }
    }

    fn char_type(&self, c: char) -> u8 {
        if let Some(t) = self.overrides.get(&c) {
            return *t;
        }
        match unicode::class(c) {
            Class::Upper => UPPER,
            Class::Lower => LOWER,
            Class::OtherLetter | Class::Nonspacing | Class::Spacing | Class::Enclosing => ALPHA,
            Class::Digit => DIGIT,
            Class::OtherNumber | Class::Other => DELIM,
        }
    }

    /// Splits a word at delimiters, letter/digit changes and lower-to-upper case changes.
    fn split<'a>(&self, word: &'a str) -> Vec<(&'a str, u8)> {
        let is_break = |last: u8, cur: u8| last & cur == 0 && !(last == UPPER && cur & ALPHA != 0);
        let mut parts: Vec<(&str, u8)> = Vec::new();
        let mut start: Option<usize> = None;
        let mut last = 0;
        for (i, c) in word.char_indices() {
            let t = self.char_type(c);
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

    /// The parts a word produces and its catenations.
    fn expand<'a>(&self, text: &'a str) -> (Vec<&'a str>, Vec<String>) {
        let parts = self.split(text);
        let single = parts.len() == 1;
        let (mut out, mut words, mut nums) = (Vec::new(), Vec::new(), Vec::new());
        for (p, t) in parts {
            if t == DIGIT {
                if self.gen_nums || single {
                    out.push(p);
                }
                nums.push(p);
            } else {
                if self.gen_words || single {
                    out.push(p);
                }
                words.push(p);
            }
        }
        let mut cats = Vec::new();
        if self.cat_words && words.len() > 1 {
            cats.push(words.concat());
        }
        if self.cat_nums && nums.len() > 1 {
            cats.push(nums.concat());
        }
        (out, cats)
    }

    fn apply(&self, stream: Stream) -> Stream {
        let mut result = Stream::new();
        for pos in stream {
            let mut rows: Vec<Vec<Tok>> = Vec::new();
            let mut extras: Vec<Tok> = Vec::new();
            for t in pos {
                let (parts, cats) = self.expand(&t.text);
                for (i, p) in parts.into_iter().enumerate() {
                    while rows.len() <= i {
                        rows.push(Vec::new());
                    }
                    if let Some(row) = rows.get_mut(i) {
                        row.push(Tok::new(p, t.kw));
                    }
                }
                extras.extend(cats.into_iter().map(|c| Tok::new(c, t.kw)));
            }
            if rows.is_empty() {
                rows.push(Vec::new());
            }
            if let Some(first) = rows.first_mut() {
                first.extend(extras);
            }
            result.extend(rows.into_iter().filter(|r| !r.is_empty()));
        }
        result
    }
}

/// Replaces `\uXXXX` escapes, as Python's `_unescape`.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("\\u") {
        out.push_str(rest.get(..at).unwrap_or_default());
        let hex = rest
            .get(at + 2..at + 6)
            .filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()));
        if let Some(c) = hex
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .and_then(char::from_u32)
        {
            out.push(c);
            rest = rest.get(at + 6..).unwrap_or_default();
        } else {
            out.push_str("\\u");
            rest = rest.get(at + 2..).unwrap_or_default();
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(name: &str, attrs: &[(&str, &str)]) -> Component {
        Component {
            name: name.to_lowercase(),
            attrs: attrs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// A temporary conf directory with the given files.
    struct Conf(PathBuf);

    impl Conf {
        fn new(tag: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("minisolr-analysis-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            for (name, body) in files {
                std::fs::write(dir.join(name), body).expect("write");
            }
            Self(dir)
        }

        fn res(&self) -> Resources {
            Resources::new(Some(&self.0))
        }
    }

    impl Drop for Conf {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn en_splitting(res: &Resources, index_time: bool) -> Analyzer {
        let cat = if index_time { "1" } else { "0" };
        let mut filters = Vec::new();
        if !index_time {
            filters.push(comp(
                "synonymGraph",
                &[
                    ("synonyms", "synonyms.txt"),
                    ("ignoreCase", "true"),
                    ("expand", "true"),
                ],
            ));
        }
        filters.extend([
            comp("icuFolding", &[]),
            comp(
                "wordDelimiterGraph",
                &[
                    ("types", "wordtypes.txt"),
                    ("generateWordParts", "1"),
                    ("generateNumberParts", "1"),
                    ("catenateWords", cat),
                    ("catenateNumbers", cat),
                ],
            ),
            comp("lowercase", &[]),
            comp("keywordMarker", &[("protected", "protwords.txt")]),
            comp("porterStem", &[]),
            comp("flattenGraph", &[]),
        ]);
        let (a, skipped) = Analyzer::build(&comp("whitespace", &[]), &filters, res);
        assert!(skipped.is_empty());
        a
    }

    const SYNONYMS: &str = "# comment\naaafoo => aaabar\nbbbfoo => bbbfoo bbbbar\nfooaaa,baraaa,bazaaa\nGB,gib,gigabyte,gigabytes\nTelevision, Televisions, TV, TVs\n";

    #[test]
    fn folds_accents_and_ligatures() {
        assert_eq!(fold("Åström Straße"), "astrom strasse");
        assert_eq!(fold("Æsop ẞ"), "aesop ss");
        // Telugu: the spacing vowel sign U (Mc) stays, only nonspacing marks go.
        assert_eq!(fold("\u{0C30}\u{0C41}"), "\u{0C30}\u{0C41}");
        assert_eq!(fold("e\u{0301}"), "e");
    }

    #[test]
    fn standard_tokenizer_joins_apostrophes_and_dots() {
        assert_eq!(
            standard_tokenize("Gulliver's Travels, U.S.A."),
            ["Gulliver's", "Travels", "U.S.A"]
        );
        assert_eq!(standard_tokenize("日本語"), ["日", "本", "語"]);
        assert_eq!(standard_tokenize("Ⓐb ²x"), ["b", "²x"]);
    }

    #[test]
    fn word_delimiter_splits_and_catenates() {
        let conf = Conf::new("wdgf", &[("wordtypes.txt", "\\u0023 => ALPHA\n")]);
        let index = en_splitting(&conf.res(), true);
        let got = index.index("Wi-Fi 802.11b C# o'neil's");
        assert_eq!(got.main, ["wi", "fi", "802", "11", "b", "c#", "o", "neil"]);
        assert_eq!(got.extra, ["wifi", "80211", "oneil"]);
        let query = en_splitting(&conf.res(), false);
        assert_eq!(query.query("Wi-Fi"), [["wi"], ["fi"]]);
        assert_eq!(
            query.query("PowerShot"),
            [["powershot"]],
            "folding lowercases first"
        );
        assert_eq!(query.query("C#"), [["c#"]]);
    }

    #[test]
    fn word_delimiter_options_without_parts() {
        let f = comp(
            "wordDelimiterGraph",
            &[
                ("generateWordParts", "0"),
                ("generateNumberParts", "0"),
                ("catenateWords", "1"),
                ("catenateNumbers", "1"),
            ],
        );
        let (a, _) = Analyzer::build(&comp("whitespace", &[]), &[f], &Resources::default());
        let got = a.index("Wi-Fi 1-2-3 x");
        assert_eq!(got.main, ["WiFi", "123", "x"]);
        assert!(got.extra.is_empty());
        assert!(
            a.query("a-1").is_empty(),
            "single parts are neither kept nor catenated"
        );
    }

    #[test]
    fn protected_words_come_from_the_file() {
        let conf = Conf::new("prot", &[("protwords.txt", "# c\ndontstems\n zwhacky \n")]);
        let a = en_splitting(&conf.res(), false);
        assert_eq!(a.query("dontstems running"), [["dontstems"], ["run"]]);
        let none = en_splitting(&Resources::default(), false);
        assert_eq!(none.query("dontstems"), [["dontstem"]]);
        assert_eq!(a.query("o'neil's"), [["o"], ["neil"]]);
    }

    #[test]
    fn stop_filter_reads_the_named_file() {
        let conf = Conf::new("stop", &[("stop_en.txt", "the\nOf\n")]);
        let build = |ic: &str| {
            Analyzer::build(
                &comp("standard", &[]),
                &[
                    comp("stop", &[("words", "stop_en.txt"), ("ignoreCase", ic)]),
                    comp("lowercase", &[]),
                ],
                &conf.res(),
            )
            .0
        };
        assert_eq!(
            build("true").index("The Lord of the Rings").main,
            ["lord", "rings"]
        );
        assert_eq!(
            build("false").index("The Lord of the Rings").main,
            ["the", "lord", "of", "rings"]
        );
        let missing = Analyzer::build(&comp("standard", &[]), &[comp("stop", &[])], &conf.res()).0;
        assert_eq!(missing.index("the").main, ["the"]);
    }

    #[test]
    fn synonyms_follow_python() {
        let conf = Conf::new("syn", &[("synonyms.txt", SYNONYMS)]);
        let syn = |expand: &str| {
            Analyzer::build(
                &comp("whitespace", &[]),
                &[
                    comp(
                        "synonymGraph",
                        &[("ignoreCase", "true"), ("expand", expand)],
                    ),
                    comp("lowercase", &[]),
                ],
                &conf.res(),
            )
            .0
        };
        let q = syn("true");
        assert_eq!(q.query("aaafoo"), [["aaabar"]]);
        assert_eq!(
            q.query("bbbfoo"),
            [["bbbfoo"]],
            "multi-word rules are ignored"
        );
        assert_eq!(
            q.query("GB x"),
            [vec!["gb", "gib", "gigabyte", "gigabytes"], vec!["x"]]
        );
        assert_eq!(q.query("TVs"), [["tvs", "television", "televisions", "tv"]]);
        assert_eq!(q.index("tv").extra, ["television", "televisions", "tvs"]);
        assert_eq!(syn("false").query("gigabytes"), [["gb"]]);
        let (case, _) = Analyzer::build(
            &comp("whitespace", &[]),
            &[comp("synonymGraph", &[])],
            &conf.res(),
        );
        assert_eq!(case.query("Gb"), [["Gb"]], "case-sensitive");
        assert_eq!(case.query("GB").first().map(Vec::len), Some(4));
    }

    #[test]
    fn synonyms_then_word_delimiter_keep_alternatives_aligned() {
        let conf = Conf::new("synwd", &[("synonyms.txt", "wi-fi, wlan\n")]);
        let a = en_splitting(&conf.res(), false);
        assert_eq!(a.query("Wi-Fi"), [vec!["wi", "wlan"], vec!["fi"]]);
    }

    #[test]
    fn minimal_stem_and_duplicates() {
        for (w, s) in [
            ("cats", "cat"),
            ("ponies", "pony"),
            ("toes", "toes"),
            ("bus", "bus"),
            ("glass", "glass"),
            ("series", "sery"),
            ("is", "is"),
        ] {
            assert_eq!(minimal_stem(w), s, "{w}");
        }
        let (a, _) = Analyzer::build(
            &comp("keyword", &[]),
            &[comp("removeDuplicates", &[])],
            &Resources::default(),
        );
        assert_eq!(a.query(""), Vec::<Vec<String>>::new());
        assert_eq!(Analyzer::raw().query(""), [[""]]);
    }

    #[test]
    fn unknown_components_are_reported_and_skipped() {
        let (a, skipped) = Analyzer::build(
            &comp("japanese", &[]),
            &[comp("lowercase", &[]), comp("germanLightStem", &[])],
            &Resources::default(),
        );
        assert_eq!(
            skipped,
            ["tokenizer japanese (standard used)", "germanlightstem"]
        );
        assert_eq!(a.index("Häuser X").main, ["häuser", "x"]);
        assert_eq!(unescape("\\u00A9 \\u12 x"), "\u{A9} \\u12 x");
    }
}
