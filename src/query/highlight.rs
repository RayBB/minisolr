//! `hl=true` and `spellcheck=true`, as Python minisolr's `highlighting` and `spellcheck`.
//!
//! Highlighting: the terms of `hl.q` (or `q`, with local params and `$param`s blanked)
//! after each `hl.fl` field's query analyzer; every word of the field's values (in the
//! row with copyFields applied) whose own analysis shares a term is wrapped in
//! `<em>...</em>`, and each value with a match is one snippet (the whole value), at most
//! `hl.snippets` (default 1) per field. Like Python, `hl.fragsize`, `hl.simple.pre`/
//! `hl.simple.post`, `hl.requireFieldMatch` and the other `hl.*` options are ignored.
//!
//! Spellcheck: each word of `spellcheck.q` (or of `userWorkQuery` / `q` with local params,
//! `$param`s and punctuation blanked) that folds to 4 or more characters and is not in
//! the dictionary ([`Index::term_dict`]) gets up to `spellcheck.count` (default 5)
//! suggestions from `difflib.get_close_matches` with cutoff 0.6. Collations are always
//! empty.

use serde_json::{Map, Value, json};

use super::Params;
use super::pyre::{py_head, py_int, scrub, word_spans};
use crate::Error;
use crate::analysis::{Analyzer, fold};
use crate::difflib::get_close_matches;
use crate::index::{Index, values};
use crate::schema::Kind;
use crate::unicode;

fn int_param(params: &Params, name: &str, default: i64) -> Result<i64, Error> {
    params.get(name).map_or(Ok(default), |v| {
        py_int(v).ok_or_else(|| {
            Error::Other(format!(
                "ValueError: invalid literal for int() with base 10: {v:?} ({name})"
            ))
        })
    })
}

/// Every alternative of every position of `text`.
fn terms(analyzer: &Analyzer, text: &str) -> Vec<String> {
    analyzer.query(text).into_iter().flatten().collect()
}

/// The `highlighting` section for the returned page (`ords` in page order), keyed by
/// each document's `key`.
///
/// # Errors
/// A `hl.snippets` that is not an integer.
pub(super) fn highlighting(store: &Index, ords: &[usize], params: &Params) -> Result<Value, Error> {
    let hq = params
        .get("hl.q")
        .filter(|s| !s.is_empty())
        .or_else(|| params.get("q"))
        .unwrap_or_default();
    let plain = scrub(hq, false);
    let snippets = int_param(params, "hl.snippets", 1)?;
    let fields: Vec<&str> = params
        .get("hl.fl")
        .unwrap_or_default()
        .split(',')
        .map(|f| f.trim_matches(unicode::is_space))
        .filter(|f| !f.is_empty())
        .collect();
    // The terms only depend on the field, so they are worked out once per field.
    let mut wanted: Vec<(&str, &Analyzer, Vec<String>)> = Vec::new();
    for name in &fields {
        if !name.contains('*') && store.schema.get(name).is_none() {
            return Err(Error::BadRequest(format!("undefined field {name}")));
        }
        let Some(def) = store.schema.get(name).filter(|f| f.kind == Kind::Text) else {
            continue;
        };
        let Some(t) = store.schema.field_type(&def) else {
            continue;
        };
        if t.query_analyzer.is_none() {
            continue;
        }
        let hl_terms = terms(&t.query, &plain);
        if !hl_terms.is_empty() {
            wanted.push((name, &t.query, hl_terms));
        }
    }
    let empty = Map::new();
    let mut out = Map::new();
    for &ord in ords {
        let row = store
            .schema
            .effective(store.row(ord).as_object().unwrap_or(&empty));
        let mut entry = Map::new();
        for (name, analyzer, hl_terms) in &wanted {
            let Some(raw) = row.get(*name) else {
                continue;
            };
            let snips: Vec<Value> = values(raw)
                .filter_map(|v| mark(&super::py_str(v), analyzer, hl_terms).map(Value::String))
                .collect();
            if !snips.is_empty() {
                entry.insert((*name).to_owned(), Value::Array(py_head(snips, snippets)));
            }
        }
        if !entry.is_empty()
            && let Some(key) = row.get("key")
        {
            out.insert(super::py_str(key), Value::Object(entry));
        }
    }
    Ok(Value::Object(out))
}

/// The value with every matching word wrapped in `<em>`, or `None` when no word matches.
fn mark(value: &str, analyzer: &Analyzer, hl_terms: &[String]) -> Option<String> {
    let mut out = String::with_capacity(value.len() + 16);
    let mut matched = false;
    let mut last = 0;
    for (start, end) in word_spans(value) {
        let (Some(before), Some(word)) = (value.get(last..start), value.get(start..end)) else {
            continue;
        };
        out.push_str(before);
        if terms(analyzer, word).iter().any(|t| hl_terms.contains(t)) {
            matched = true;
            out.push_str("<em>");
            out.push_str(word);
            out.push_str("</em>");
        } else {
            out.push_str(word);
        }
        last = end;
    }
    out.push_str(value.get(last..).unwrap_or_default());
    matched.then_some(out)
}

/// The `spellcheck` section: `{"suggestions": [word, {...}, ...], "collations": []}`.
///
/// # Errors
/// A `spellcheck.count` that is not an integer, or one below 1 when a word needs
/// suggestions (Python's `get_close_matches` raises then).
pub(super) fn spellcheck(store: &Index, params: &Params) -> Result<Value, Error> {
    let count = int_param(params, "spellcheck.count", 5)?;
    let q = match params.get("spellcheck.q").filter(|s| !s.is_empty()) {
        Some(q) => q.to_owned(),
        None => scrub(
            params
                .get("userWorkQuery")
                .filter(|s| !s.is_empty())
                .or_else(|| params.get("q"))
                .unwrap_or_default(),
            true,
        ),
    };
    let mut suggestions = Vec::new();
    let mut pos = 0;
    for w in unicode::split_whitespace(&q) {
        let start = q
            .get(pos..)
            .and_then(|rest| rest.find(w))
            .map_or(pos, |i| pos + i);
        pos = start + w.len();
        let folded = fold(w);
        if folded.chars().count() < 4 || store.term_dict.binary_search(&folded).is_ok() {
            continue;
        }
        let n = usize::try_from(count)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Other(format!("ValueError: n must be > 0: {count}")))?;
        let close = get_close_matches(&folded, store.term_dict.iter().map(String::as_str), n, 0.6);
        if !close.is_empty() {
            let start_char = q.get(..start).map_or(0, |s| s.chars().count());
            suggestions.push(Value::String(w.to_owned()));
            suggestions.push(json!({
                "numFound": close.len(),
                "startOffset": start_char,
                "endOffset": start_char + w.chars().count(),
                "suggestion": close,
            }));
        }
    }
    Ok(json!({"suggestions": suggestions, "collations": []}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Schema;
    use std::path::Path;
    use std::sync::Arc;

    fn store() -> Index {
        let conf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/conf");
        let schema = Schema::load(&conf).expect("schema");
        let docs = [
            json!({"key": "/works/OL1W", "type": "work", "title": "The Adventures of Tom Sawyer",
                   "subject": ["Fiction", "Juvenile fiction", "Sawyer, Tom (Fictitious character)", "Boys"]}),
            json!({"key": "/works/OL2W", "type": "work", "title": "Alice's Adventures", "subject": ["Fantasy"],
                   "author_name": ["Lewis Carroll"]}),
            json!({"key": "/works/OL3W", "type": "work", "title": "Running wild"}),
        ];
        Index::from_docs(
            Arc::new(schema),
            docs.into_iter().map(Arc::new).collect(),
            0,
        )
        .expect("index")
    }

    fn params(pairs: &[(&str, &str)]) -> Params {
        Params::new(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn highlights_matching_words_per_value() {
        let s = store();
        let hl = highlighting(
            &s,
            &[0, 1, 2],
            &params(&[
                ("q", "sawyer"),
                ("hl.fl", "title, subject"),
                ("hl.snippets", "10"),
            ]),
        )
        .expect("hl");
        assert_eq!(
            hl,
            json!({"/works/OL1W": {
                "title": ["The Adventures of Tom <em>Sawyer</em>"],
                "subject": ["<em>Sawyer</em>, Tom (Fictitious character)"]
            }})
        );
    }

    #[test]
    fn snippets_option_and_hl_q() {
        let s = store();
        let p = |snips: &str| {
            params(&[
                ("q", "x"),
                ("hl.q", "{!edismax}fiction $x"),
                ("hl.fl", "subject"),
                ("hl.snippets", snips),
            ])
        };
        let one = highlighting(&s, &[0], &p("1")).expect("hl");
        assert_eq!(
            one,
            json!({"/works/OL1W": {"subject": ["<em>Fiction</em>"]}})
        );
        let all = highlighting(&s, &[0], &p("10")).expect("hl");
        assert_eq!(
            all["/works/OL1W"]["subject"],
            json!(["<em>Fiction</em>", "Juvenile <em>fiction</em>"]),
            "fictitious stems to fictiti"
        );
        let none = highlighting(&s, &[0], &p("0")).expect("hl");
        assert_eq!(
            none,
            json!({"/works/OL1W": {"subject": []}}),
            "Python keeps the empty list"
        );
        let negative = highlighting(&s, &[0], &p("-1")).expect("hl");
        assert_eq!(
            negative["/works/OL1W"]["subject"].as_array().map(Vec::len),
            Some(1)
        );
        assert!(highlighting(&s, &[0], &p("many")).is_err());
    }

    #[test]
    fn highlighting_an_undefined_field_is_a_bad_request() {
        let s = store();
        let err = highlighting(&s, &[0], &params(&[("q", "tom"), ("hl.fl", "title,nope")]))
            .expect_err("undefined field");
        assert!(matches!(&err, Error::BadRequest(m) if m == "undefined field nope"));
    }

    #[test]
    fn highlighting_stems_and_skips_non_text_fields() {
        let s = store();
        let hl = highlighting(
            &s,
            &[0, 1, 2],
            &params(&[("q", "run adventure"), ("hl.fl", "title,key,type")]),
        )
        .expect("hl");
        assert_eq!(
            hl,
            json!({
                "/works/OL1W": {"title": ["The <em>Adventures</em> of Tom Sawyer"]},
                "/works/OL2W": {"title": ["Alice's <em>Adventures</em>"]},
                "/works/OL3W": {"title": ["<em>Running</em> wild"]}
            })
        );
        let ignored = [
            ("q", "run"),
            ("hl.fl", "title"),
            ("hl.simple.pre", "<b>"),
            ("hl.fragsize", "5"),
            ("hl.requireFieldMatch", "true"),
        ];
        let hl = highlighting(&s, &[2], &params(&ignored)).expect("hl");
        assert_eq!(
            hl,
            json!({"/works/OL3W": {"title": ["<em>Running</em> wild"]}})
        );
        let no_fl = highlighting(&s, &[2], &params(&[("q", "run")])).expect("hl");
        assert_eq!(no_fl, json!({}));
    }

    #[test]
    fn spellcheck_suggests_close_dictionary_words() {
        let s = store();
        assert!(
            s.term_dict.contains(&"sawyer".to_owned())
                && s.term_dict.contains(&"alice's".to_owned())
        );
        assert!(
            !s.term_dict.contains(&"tom".to_owned()),
            "words of 3 characters are left out"
        );
        let sc = spellcheck(
            &s,
            &params(&[
                ("q", "{!edismax v=$q} Sawyre adventurs tom"),
                ("spellcheck.count", "3"),
            ]),
        )
        .expect("sc");
        assert_eq!(
            sc,
            json!({"suggestions": [
                "Sawyre", {"numFound": 1, "startOffset": 2, "endOffset": 8, "suggestion": ["sawyer"]},
                "adventurs", {"numFound": 1, "startOffset": 9, "endOffset": 18, "suggestion": ["adventures"]}
            ], "collations": []})
        );
        let ok =
            spellcheck(&s, &params(&[("userWorkQuery", "Sawyer"), ("q", "zzzz")])).expect("sc");
        assert_eq!(ok["suggestions"], json!([]));
        let sq = spellcheck(&s, &params(&[("spellcheck.q", "carrol!"), ("q", "x")])).expect("sc");
        assert_eq!(
            sq["suggestions"],
            json!(["carrol!", {"numFound": 1, "startOffset": 0, "endOffset": 7, "suggestion": ["carroll"]}]),
            "spellcheck.q is not scrubbed"
        );
        assert!(spellcheck(&s, &params(&[("q", "sawyre"), ("spellcheck.count", "0")])).is_err());
        assert!(spellcheck(&s, &params(&[("q", "ok"), ("spellcheck.count", "0")])).is_ok());
    }
}
