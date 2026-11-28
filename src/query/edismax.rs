//! The edismax parser as Python minisolr implements it: the query is parsed with the
//! Lucene rules, bare terms search every `qf` field (a `DisjunctionMax`, tie 0), the
//! default operator is `q.op`, `mm` sets how many top-level optional clauses must match,
//! and with two or more words `pf`, `pf2` and `pf3` add optional phrase clauses that only
//! change the ranking.
//!
//! `boost=` is collected into [`Compiler::boosts`] and `bf` is read by `select`, which
//! rescore the hits after the search. `ps` (slop of the `pf*` phrases) and `qs` (slop
//! of quoted phrases without `~n`) default to 0, which is what Python always uses.

use tantivy::query::{BooleanQuery, BoostQuery, EmptyQuery, Occur, Query};

use super::Params;
use super::ast::LocalParams;
use super::compile::{Compiler, Ctx, LeafOpts, QueryResult, all_docs};
use super::localparams::deref;
use crate::Error;

/// An edismax `mm` (minimum should match) spec, or the default derived from `q.op`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Mm<'c> {
    /// The `mm` parameter, if given.
    pub spec: Option<&'c str>,
    /// `q.op=AND`: without a spec every clause is required, otherwise none.
    pub and_op: bool,
}

impl Mm<'_> {
    /// How many of `n` optional clauses must match (may exceed `n` for odd specs).
    pub(super) fn required(&self, n: usize) -> usize {
        let Some(spec) = self.spec else {
            return if self.and_op { n } else { 0 };
        };
        let mut rule = spec;
        let n_text;
        if spec.contains('<') {
            // `3<90% 5<-1`: the rule after the largest limit below n applies; at or
            // below every limit all clauses are required.
            n_text = n.to_string();
            rule = &n_text;
            for part in spec.split_whitespace() {
                let (limit, r) = part.split_once('<').unwrap_or((part, ""));
                match limit.parse::<usize>() {
                    Ok(limit) if n > limit => rule = r,
                    Ok(_) => {}
                    Err(_) => return n,
                }
            }
        }
        let signed_n = i64::try_from(n).unwrap_or(i64::MAX);
        let value = if let Some(pct) = rule.strip_suffix('%') {
            let Ok(v) = pct.parse::<i64>() else {
                return n;
            };
            let k = signed_n.saturating_mul(v.abs()) / 100;
            if v < 0 { (signed_n - k).max(0) } else { k }
        } else {
            let Ok(v) = rule.parse::<i64>() else {
                return n;
            };
            if v < 0 {
                (signed_n + v).max(0)
            } else {
                v.min(signed_n)
            }
        };
        usize::try_from(value).unwrap_or(0)
    }
}

/// Parses `name^boost name ...`; a missing boost is 1. A repeated name keeps its first
/// position and its last boost.
///
/// # Errors
/// [`Error::Unsupported`] when a boost is not a number.
pub(super) fn parse_boosts(s: &str) -> Result<Vec<(&str, f32)>, Error> {
    let mut out: Vec<(&str, f32)> = Vec::new();
    for item in s.split_whitespace() {
        let (name, boost) = item.split_once('^').unwrap_or((item, ""));
        let boost = if boost.is_empty() {
            1.0
        } else {
            boost
                .parse()
                .map_err(|_| Error::Unsupported(format!("field boost {item:?}")))?
        };
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = boost,
            None => out.push((name, boost)),
        }
    }
    Ok(out)
}

/// The words `pf`, `pf2` and `pf3` are built from: the query without `field:` prefixes,
/// split at whitespace, quotes and parentheses, minus operators and `+ - !` clauses.
#[must_use]
pub(super) fn phrase_words(v: &str) -> Vec<String> {
    strip_field_prefixes(v)
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .filter(|w| {
            !["AND", "OR", "NOT"]
                .iter()
                .any(|op| w.eq_ignore_ascii_case(op))
                && !w.starts_with(['+', '-', '!'])
        })
        .map(str::to_owned)
        .collect()
}

/// Removes each `\b\w+:` (a whole word directly followed by a colon).
fn strip_field_prefixes(v: &str) -> String {
    let is_word = |c: char| c == '_' || c.is_alphanumeric();
    let mut out = String::with_capacity(v.len());
    let mut prev_word = false;
    let mut rest = v;
    while let Some(c) = rest.chars().next() {
        if is_word(c) && !prev_word {
            let run = rest.find(|d: char| !is_word(d)).unwrap_or(rest.len());
            let (word, after) = rest.split_at(run);
            if let Some(after_colon) = after.strip_prefix(':') {
                rest = after_colon;
                prev_word = false;
            } else {
                out.push_str(word);
                rest = after;
                prev_word = true;
            }
            continue;
        }
        out.push(c);
        prev_word = is_word(c);
        rest = rest.get(c.len_utf8()..).unwrap_or_default();
    }
    out
}

/// A parameter of a local-params edismax (`local`), or of the request for a top-level
/// `defType=edismax` (`local: None`).
fn own<'x>(local: Option<&'x LocalParams<'x>>, params: &'x Params, key: &str) -> Option<&'x str> {
    match local {
        Some(l) => l.get(key),
        None => params.get(key),
    }
}

/// Like [`own`], falling back to the request parameter.
fn inherited<'x>(
    local: Option<&'x LocalParams<'x>>,
    params: &'x Params,
    key: &str,
) -> Option<&'x str> {
    own(local, params, key).or_else(|| params.get(key))
}

fn slop(text: Option<&str>) -> u32 {
    text.and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

impl Compiler<'_> {
    /// Compiles `v` with edismax; `local` holds the `{!edismax ...}` params, or `None`
    /// for `defType=edismax` on the request.
    pub(super) fn edismax(&mut self, v: &str, local: Option<&LocalParams<'_>>) -> QueryResult {
        let params = self.params;
        let mut qf = parse_boosts(deref(params, inherited(local, params, "qf").unwrap_or("")))?;
        if qf.is_empty() {
            qf.push(("title", 1.0));
        }
        if let Some((name, _)) = qf
            .iter()
            .find(|(n, _)| !n.contains('*') && self.store.schema.get(n).is_none())
        {
            return Err(Error::BadRequest(format!(
                "org.apache.solr.search.SyntaxError: Query Field '{name}' is not a valid field name"
            )));
        }
        let and_op = inherited(local, params, "q.op") == Some("AND");
        let sub = Ctx {
            default_fields: Some(&qf),
            and_op,
            mm: Some(Mm {
                spec: inherited(local, params, "mm"),
                and_op,
            }),
            phrase_slop: slop(inherited(local, params, "qs")),
        };
        if let Some(boost) = own(local, params, "boost") {
            self.boosts.push(deref(params, boost).to_owned());
        }
        let v = v.trim();
        if matches!(v, "" | "*" | "*:*") {
            return Ok(Some(all_docs()));
        }
        let main = self
            .compile_str(v, sub)?
            .unwrap_or_else(|| Box::new(EmptyQuery));
        let words = phrase_words(v);
        if words.len() < 2 {
            return Ok(Some(main));
        }
        let mut parts: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, main)];
        let opts = LeafOpts {
            phrase: true,
            slop: slop(inherited(local, params, "ps")),
            fuzzy: None,
        };
        for (key, size) in [("pf", 0), ("pf2", 2), ("pf3", 3)] {
            let Some(spec) = own(local, params, key) else {
                continue;
            };
            for (name, boost) in parse_boosts(deref(params, spec))? {
                if self.store.schema.get(name).is_none() {
                    continue;
                }
                let shingles: Vec<String> = if size == 0 {
                    vec![words.join(" ")]
                } else {
                    words.windows(size).map(|w| w.join(" ")).collect()
                };
                for text in shingles {
                    if let Some(q) = self.leaf(name, &text, sub, opts)? {
                        parts.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
                    }
                }
            }
        }
        Ok(Some(if parts.len() > 1 {
            Box::new(BooleanQuery::new(parts))
        } else {
            parts
                .pop()
                .map_or_else(|| Box::new(EmptyQuery) as Box<dyn Query>, |(_, q)| q)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mm(spec: Option<&str>, and_op: bool, n: usize) -> usize {
        Mm { spec, and_op }.required(n)
    }

    #[test]
    fn mm_specs() {
        assert_eq!(mm(None, true, 3), 3);
        assert_eq!(mm(None, false, 3), 0);
        assert_eq!(mm(Some("2"), false, 3), 2);
        assert_eq!(mm(Some("5"), false, 3), 3);
        assert_eq!(mm(Some("-1"), false, 3), 2);
        assert_eq!(mm(Some("100%"), false, 3), 3);
        assert_eq!(mm(Some("50%"), false, 3), 1);
        assert_eq!(mm(Some("-25%"), false, 4), 3);
        assert_eq!(mm(Some("2<-1 5<80%"), false, 2), 2);
        assert_eq!(mm(Some("2<-1 5<80%"), false, 4), 3);
        assert_eq!(mm(Some("2<-1 5<80%"), false, 10), 8);
        assert_eq!(mm(Some("x"), false, 4), 4);
    }

    #[test]
    fn boosts_lists() {
        assert_eq!(
            parse_boosts("a^40 b c^2.5 a^3").expect("parses"),
            [("a", 3.0), ("b", 1.0), ("c", 2.5)]
        );
        assert!(parse_boosts("a^x").is_err());
    }

    #[test]
    fn words_for_phrase_fields() {
        assert_eq!(
            phrase_words(r#"alternative_title:"green eggs" AND author_name:seuss"#),
            ["green", "eggs", "seuss"]
        );
        assert_eq!(
            phrase_words("+alternative_title:\"green eggs\" "),
            ["green", "eggs"]
        );
        assert_eq!(
            phrase_words("lewis -carroll (alice or x)"),
            ["lewis", "alice", "x"]
        );
        assert_eq!(
            phrase_words("a:b:c gulliver's travels"),
            ["c", "gulliver's", "travels"]
        );
    }
}
