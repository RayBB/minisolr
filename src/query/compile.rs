//! Turns the AST into Tantivy queries, mirroring Python minisolr's `Parser.expr`, `leaf`,
//! `range_query` and `dismax_leaf`.
//!
//! A compiled clause is `Ok(None)` when it vanishes (analysis left no tokens, a range on
//! an unknown field under edismax); the clause is then dropped together with its
//! operator, as in the Python code. A clause that cannot match is an [`EmptyQuery`].
//! Features the Python code only warns about (an undefined field, a bare term with no
//! default field, a range on a text field, an unknown `{!type}`) are
//! [`Error::Unsupported`] here, so the whole request answers empty with a warning.

use std::ops::Bound;

use tantivy::Term;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, DisjunctionMaxQuery, EmptyQuery,
    FuzzyTermQuery, Occur, PhraseQuery, Query, RangeQuery, RegexQuery, TermQuery, TermSetQuery,
};
use tantivy::schema::{Field, IndexRecordOption};

use super::Params;
use super::ast::{Boost, Clause, Conj, Node, ParseError, Prefix, RangeBounds};
use super::edismax::Mm;
use super::parser;
use crate::Error;
use crate::analysis::fold;
use crate::schema::Kind;
use crate::store::Store;

/// A compiled clause: `None` when it vanished.
pub(super) type QueryResult = Result<Option<Box<dyn Query>>, Error>;

/// How deep local params, `_query_` and `{!parent}` may nest before giving up.
const MAX_DEPTH: u32 = 32;

/// Most `mm` combinations expanded into explicit clauses, as in Python.
const MAX_MM_COMBINATIONS: usize = 200;

/// The settings a query string is compiled with.
#[derive(Clone, Copy, Debug)]
pub(super) struct Ctx<'c> {
    /// Fields and boosts for bare terms (edismax `qf`, `df`, or `field:(...)`).
    pub default_fields: Option<&'c [(&'c str, f32)]>,
    /// Default operator `AND` (otherwise `OR`).
    pub and_op: bool,
    /// edismax `mm`; applies to the top level of the edismax query and its plain groups.
    pub mm: Option<Mm<'c>>,
    /// Slop for quoted phrases without `~n` (edismax `qs`).
    pub phrase_slop: u32,
}

impl<'c> Ctx<'c> {
    /// No default fields, `OR`, no `mm`: how `fq` and the lucene parser start.
    pub(super) fn lucene(default_fields: Option<&'c [(&'c str, f32)]>, and_op: bool) -> Self {
        Self {
            default_fields,
            and_op,
            mm: None,
            phrase_slop: 0,
        }
    }

    /// The same settings without `mm` (Python's `Ctx.child`).
    pub(super) fn child(self) -> Self {
        Self { mm: None, ..self }
    }
}

/// Options of a single field clause.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LeafOpts {
    /// Quoted: always a phrase, no wildcards.
    pub phrase: bool,
    /// Phrase slop.
    pub slop: u32,
    /// Fuzzy edit distance.
    pub fuzzy: Option<u32>,
}

/// Compiles the query strings of one request.
pub(crate) struct Compiler<'p> {
    pub(super) store: &'p Store,
    pub(super) params: &'p Params,
    /// Step 3 seam: function-query `boost` expressions met while compiling (edismax
    /// `boost=`, already dereferenced). Python multiplies each hit's score by every one of
    /// them after the search; for now they are collected and ignored.
    pub(super) boosts: Vec<String>,
    depth: u32,
}

impl<'p> Compiler<'p> {
    pub(crate) fn new(store: &'p Store, params: &'p Params) -> Self {
        Self {
            store,
            params,
            boosts: Vec::new(),
            depth: 0,
        }
    }

    /// The query for `q` plus filters, as Python's `run_query` builds it: `q` with
    /// `defType` (edismax over the request params, or lucene with `df` and `q.op`), each
    /// filter a required clause with score 0. An empty `q` means `*:*`.
    pub(crate) fn run_query(&mut self, q: &str, fqs: &[&str]) -> Result<Box<dyn Query>, Error> {
        let params = self.params;
        let q = if q.trim().is_empty() { "*:*" } else { q };
        let main = if matches!(params.get("defType"), Some("edismax" | "dismax")) {
            self.edismax(q, None)?
        } else {
            let df = params.get("df").map(|f| [(f, 1.0f32)]);
            let and_op = params.get("q.op") == Some("AND");
            self.compile_str(
                q,
                Ctx::lucene(df.as_ref().map(<[(&str, f32); 1]>::as_slice), and_op),
            )?
        };
        let main = main.unwrap_or_else(|| Box::new(EmptyQuery));
        if fqs.is_empty() {
            return Ok(main);
        }
        let mut parts = vec![(Occur::Must, main)];
        for fq in fqs {
            let mark = self.boosts.len();
            let filter = self.compile_str(fq, Ctx::lucene(None, false));
            self.boosts.truncate(mark);
            if let Some(f) = filter? {
                parts.push((Occur::Must, Box::new(ConstScoreQuery::new(f, 0.0))));
            }
        }
        Ok(Box::new(BooleanQuery::new(parts)))
    }

    /// Parses and compiles one query string.
    pub(super) fn compile_str(&mut self, s: &str, ctx: Ctx<'_>) -> QueryResult {
        if self.depth >= MAX_DEPTH {
            return Err(ParseError::TooDeep.into());
        }
        let schema = &self.store.schema;
        let clauses = parser::parse(s, ctx.default_fields.is_some(), &|f| {
            schema.get(f).is_some()
        })?;
        self.depth += 1;
        let out = self.clauses(&clauses, ctx);
        self.depth -= 1;
        out
    }

    /// Lucene classic boolean rules over one group, as Python's `Parser.expr`.
    fn clauses(&mut self, clauses: &[Clause<'_>], ctx: Ctx<'_>) -> QueryResult {
        let mut out: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for clause in clauses {
            let Some(q) = self.node(&clause.node, ctx)? else {
                continue;
            };
            if let Some((last, _)) = out.last_mut()
                && *last != Occur::MustNot
            {
                if clause.conj == Some(Conj::And) {
                    *last = Occur::Must;
                }
                if ctx.and_op && clause.conj == Some(Conj::Or) {
                    *last = Occur::Should;
                }
            }
            let prohibited = clause.prefix == Some(Prefix::Prohibited);
            let required = if ctx.and_op {
                !prohibited && clause.conj != Some(Conj::Or)
            } else {
                clause.prefix == Some(Prefix::Required)
                    || (clause.conj == Some(Conj::And) && !prohibited)
            };
            let occur = if prohibited {
                Occur::MustNot
            } else if required {
                Occur::Must
            } else {
                Occur::Should
            };
            out.push((occur, q));
        }
        if out.len() == 1 && out.first().is_some_and(|(o, _)| *o != Occur::MustNot) {
            return Ok(out.pop().map(|(_, q)| q));
        }
        if out.is_empty() {
            return Ok(None);
        }
        if out.iter().all(|(o, _)| *o == Occur::MustNot) {
            out.push((Occur::Must, all_docs()));
        } else if out.iter().all(|(o, _)| *o == Occur::Should) {
            let need = ctx.mm.map_or(0, |mm| mm.required(out.len()));
            if need > 1 {
                return Ok(Some(min_should(&out, need)));
            }
        }
        Ok(Some(Box::new(BooleanQuery::new(out))))
    }

    fn node(&mut self, node: &Node<'_>, ctx: Ctx<'_>) -> QueryResult {
        match node {
            Node::Group(clauses) => self.clauses(clauses, ctx),
            Node::FieldGroup { field, clauses } => {
                let df = [(field.as_ref(), 1.0f32)];
                let inner = Ctx {
                    default_fields: Some(&df),
                    mm: None,
                    ..ctx
                };
                self.clauses(clauses, inner)
            }
            Node::Term { field, text, fuzzy } => {
                let opts = LeafOpts {
                    fuzzy: *fuzzy,
                    ..LeafOpts::default()
                };
                match field {
                    Some(f) => self.leaf(f, text, ctx, opts),
                    None => self.default_fields_leaf(text, ctx, opts),
                }
            }
            Node::Phrase { field, text, slop } => {
                let opts = LeafOpts {
                    phrase: true,
                    slop: slop.unwrap_or(ctx.phrase_slop),
                    fuzzy: None,
                };
                match field {
                    Some(f) => self.leaf(f, text, ctx, opts),
                    None => self.default_fields_leaf(text, ctx, opts),
                }
            }
            Node::Range { field, bounds } => {
                if ctx.default_fields.is_some() && self.store.schema.get(field).is_none() {
                    return Ok(None);
                }
                self.range(field, bounds.as_ref())
            }
            Node::All => Ok(Some(all_docs())),
            Node::Local(local) => self.local_params(local, ctx),
            Node::Nested(s) => self.compile_str(s, ctx),
            Node::Boosted { node, boost } => Ok(self.node(node, ctx)?.map(|q| match *boost {
                Boost::Multiply(b) => Box::new(BoostQuery::new(q, b)) as Box<dyn Query>,
                Boost::Constant(c) => Box::new(ConstScoreQuery::new(q, c)),
            })),
        }
    }

    fn default_fields_leaf(&self, text: &str, ctx: Ctx<'_>, opts: LeafOpts) -> QueryResult {
        let Some(fields) = ctx.default_fields else {
            return Err(Error::Unsupported(format!(
                "bare term {text:?} without a default field"
            )));
        };
        let mut disjuncts: Vec<Box<dyn Query>> = Vec::new();
        for &(name, boost) in fields {
            let Some(q) = self.leaf(name, text, ctx, opts)? else {
                continue;
            };
            disjuncts.push(if (boost - 1.0).abs() > f32::EPSILON {
                Box::new(BoostQuery::new(q, boost))
            } else {
                q
            });
        }
        Ok(match disjuncts.len() {
            0 => None,
            1 => disjuncts.pop(),
            _ => Some(Box::new(DisjunctionMaxQuery::with_tie_breaker(
                disjuncts, 0.0,
            ))),
        })
    }

    /// One `field:text` clause (Python's `leaf`).
    pub(super) fn leaf(&self, name: &str, text: &str, ctx: Ctx<'_>, opts: LeafOpts) -> QueryResult {
        let store = self.store;
        let Some(def) = store.schema.get(name) else {
            return Err(Error::Unsupported(format!("undefined field {name}")));
        };
        let Some(indexed) = store.fields.get(name) else {
            return Ok(Some(Box::new(EmptyQuery)));
        };
        let field = indexed.handle;
        let wild = !opts.phrase && text.contains(['*', '?']);
        let raw_term = |t: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, t),
                IndexRecordOption::Basic,
            ))
        };
        match def.kind {
            Kind::Text => {
                if wild {
                    return regex(field, &wildcard_regex(&fold(text))).map(Some);
                }
                let positions = store.analyzer.query(def.chain, text);
                if let (Some(distance), [only]) = (opts.fuzzy, positions.as_slice()) {
                    return Ok(Some(fuzzy(field, only, distance)));
                }
                let auto_phrase = opts.phrase
                    || store
                        .schema
                        .field_type(&def)
                        .is_some_and(|t| t.attr_true("autoGeneratePhraseQueries", false));
                Ok(positions_query(
                    field,
                    &positions,
                    auto_phrase,
                    opts.slop,
                    ctx.and_op,
                ))
            }
            Kind::Int | Kind::Float => {
                if text == "*" {
                    return Ok(Some(f64_range(field, -1e18, 1e18, true, true)));
                }
                Ok(Some(match text.trim().parse::<f64>() {
                    Ok(x) => Box::new(TermQuery::new(
                        Term::from_field_f64(field, x),
                        IndexRecordOption::Basic,
                    )),
                    Err(_) => Box::new(EmptyQuery),
                }))
            }
            _ => {
                let lowered;
                let text = if def.kind == Kind::Bool {
                    lowered = text.to_lowercase();
                    lowered.as_str()
                } else {
                    text
                };
                if text == "*" {
                    return regex(field, ".+").map(Some);
                }
                if wild {
                    return regex(field, &wildcard_regex(text)).map(Some);
                }
                if let Some(distance) = opts.fuzzy {
                    return Ok(Some(fuzzy(field, text, distance)));
                }
                Ok(Some(raw_term(text)))
            }
        }
    }

    /// `field:[lo TO hi]` (Python's `range_query`): enum ranges follow the configured value
    /// order, numbers compare as `f64`, strings, booleans and dates as text.
    fn range(&self, name: &str, bounds: Option<&RangeBounds<'_>>) -> QueryResult {
        let store = self.store;
        let empty = || Ok(Some(Box::new(EmptyQuery) as Box<dyn Query>));
        let (Some(def), Some(indexed)) = (store.schema.get(name), store.fields.get(name)) else {
            return empty();
        };
        let Some(b) = bounds else {
            return empty();
        };
        let field = indexed.handle;
        match def.kind {
            Kind::Enum => {
                let values = store
                    .schema
                    .field_type(&def)
                    .map(|t| t.enum_values.as_slice())
                    .unwrap_or_default();
                let selected = enum_range(values, b);
                if selected.is_empty() {
                    return empty();
                }
                let terms = selected.iter().map(|v| Term::from_field_text(field, v));
                Ok(Some(Box::new(TermSetQuery::new(terms))))
            }
            Kind::Int | Kind::Float => {
                let parse = |v: &str, open: f64| {
                    if v == "*" { Ok(open) } else { v.parse::<f64>() }
                };
                let (Ok(lo), Ok(hi)) = (parse(b.lo, -1e18), parse(b.hi, 1e18)) else {
                    return empty();
                };
                Ok(Some(f64_range(
                    field,
                    lo,
                    hi,
                    b.lo_inclusive || b.lo_open(),
                    b.hi_inclusive || b.hi_open(),
                )))
            }
            Kind::Str | Kind::Bool | Kind::Date => {
                let lo = if b.lo_open() { "" } else { b.lo };
                let hi = if b.hi_open() { "\u{10FFFF}" } else { b.hi };
                let bound = |t: &str, inclusive: bool| {
                    let term = Term::from_field_text(field, t);
                    if inclusive {
                        Bound::Included(term)
                    } else {
                        Bound::Excluded(term)
                    }
                };
                Ok(Some(Box::new(RangeQuery::new(
                    bound(lo, b.lo_inclusive || b.lo_open()),
                    bound(hi, b.hi_inclusive || b.hi_open()),
                ))))
            }
            Kind::Text | Kind::SortText | Kind::Skip => Err(Error::Unsupported(format!(
                "range on {:?} field {name}",
                def.kind
            ))),
        }
    }
}

/// Every document with Solr's constant score 1, built the way Python builds it.
pub(super) fn all_docs() -> Box<dyn Query> {
    Box::new(ConstScoreQuery::new(Box::new(AllQuery), 1.0))
}

/// The enum values from `lo` to `hi` in configured order; unknown ends act as open.
fn enum_range<'v>(values: &'v [String], b: &RangeBounds<'_>) -> &'v [String] {
    let position = |v: &str| values.iter().position(|x| x == v);
    let last = values.len().checked_sub(1);
    let mut from = if b.lo_open() {
        Some(0)
    } else {
        Some(position(b.lo).unwrap_or(0))
    };
    let mut to = if b.hi_open() {
        last
    } else {
        position(b.hi).or(last)
    };
    if !b.lo_open() && !b.lo_inclusive {
        from = from.map(|a| a + 1);
    }
    if !b.hi_open() && !b.hi_inclusive {
        to = to.and_then(|z| z.checked_sub(1));
    }
    match (from, to) {
        (Some(a), Some(z)) if a <= z => values.get(a..=z).unwrap_or_default(),
        _ => &[],
    }
}

/// Analyzed positions as a query: one term, a phrase (quoted, or a type with
/// `autoGeneratePhraseQueries`), or one clause per position joined by the default operator.
fn positions_query(
    field: Field,
    positions: &[String],
    phrase: bool,
    slop: u32,
    and_op: bool,
) -> Option<Box<dyn Query>> {
    let term = |t: &str| Term::from_field_text(field, t);
    let term_query =
        |t: &str| Box::new(TermQuery::new(term(t), IndexRecordOption::WithFreqs)) as Box<dyn Query>;
    match positions {
        [] => None,
        [only] => Some(term_query(only)),
        _ if phrase => Some(Box::new(PhraseQuery::new_with_offset_and_slop(
            positions.iter().map(|t| term(t)).enumerate().collect(),
            slop,
        ))),
        _ => {
            let occur = if and_op { Occur::Must } else { Occur::Should };
            Some(Box::new(BooleanQuery::new(
                positions.iter().map(|t| (occur, term_query(t))).collect(),
            )))
        }
    }
}

/// At least `need` of the optional clauses, spelled out as the first
/// [`MAX_MM_COMBINATIONS`] combinations of `need` required clauses (Python's `min_should`).
fn min_should(clauses: &[(Occur, Box<dyn Query>)], need: usize) -> Box<dyn Query> {
    let n = clauses.len();
    let need = need.min(n);
    let mut picks: Vec<usize> = (0..need).collect();
    let mut groups: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    loop {
        let group: Vec<(Occur, Box<dyn Query>)> = picks
            .iter()
            .filter_map(|&i| clauses.get(i))
            .map(|(_, q)| (Occur::Must, q.box_clone()))
            .collect();
        groups.push((Occur::Should, Box::new(BooleanQuery::new(group))));
        if groups.len() >= MAX_MM_COMBINATIONS || !next_combination(&mut picks, n) {
            break;
        }
    }
    Box::new(BooleanQuery::new(groups))
}

/// Advances `picks` to the next combination of `0..n` in lexicographic order.
fn next_combination(picks: &mut [usize], n: usize) -> bool {
    let k = picks.len();
    let Some(i) = (0..k)
        .rev()
        .find(|&i| picks.get(i).is_some_and(|&p| p + k < n + i))
    else {
        return false;
    };
    let start = picks.get(i).copied().unwrap_or_default() + 1;
    for (offset, slot) in picks.iter_mut().skip(i).enumerate() {
        *slot = start + offset;
    }
    true
}

fn f64_range(field: Field, lo: f64, hi: f64, lo_inc: bool, hi_inc: bool) -> Box<dyn Query> {
    let bound = |x: f64, inclusive: bool| {
        let term = Term::from_field_f64(field, x);
        if inclusive {
            Bound::Included(term)
        } else {
            Bound::Excluded(term)
        }
    };
    Box::new(RangeQuery::new(bound(lo, lo_inc), bound(hi, hi_inc)))
}

fn fuzzy(field: Field, text: &str, distance: u32) -> Box<dyn Query> {
    let distance = u8::try_from(distance.min(2)).unwrap_or(2);
    Box::new(FuzzyTermQuery::new(
        Term::from_field_text(field, text),
        distance,
        true,
    ))
}

fn regex(field: Field, pattern: &str) -> Result<Box<dyn Query>, Error> {
    RegexQuery::from_pattern(pattern, field)
        .map(|q| Box::new(q) as Box<dyn Query>)
        .map_err(|e| Error::Unsupported(format!("wildcard {pattern:?}: {e}")))
}

/// A Lucene wildcard as a whole-term regex: `*` any run, `?` one character, everything
/// else literal.
#[must_use]
pub(super) fn wildcard_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '\\' | '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '#' | '&'
            | '-' | '~' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combinations_in_lexicographic_order() {
        let mut picks = vec![0, 1];
        let mut seen = vec![picks.clone()];
        while next_combination(&mut picks, 4) {
            seen.push(picks.clone());
        }
        assert_eq!(
            seen,
            [[0, 1], [0, 2], [0, 3], [1, 2], [1, 3], [2, 3]].map(Vec::from)
        );
        let mut single = vec![0, 1, 2];
        assert!(!next_combination(&mut single, 3));
    }

    #[test]
    fn enum_ranges_follow_configured_order() {
        let values: Vec<String> = ["no_ebook", "printdisabled", "borrowable", "public"]
            .map(String::from)
            .to_vec();
        let range = |lo, hi, li, hi_inc| RangeBounds {
            lo,
            hi,
            lo_inclusive: li,
            hi_inclusive: hi_inc,
        };
        assert_eq!(
            enum_range(&values, &range("borrowable", "*", true, true)),
            ["borrowable", "public"]
        );
        assert_eq!(
            enum_range(&values, &range("no_ebook", "borrowable", false, false)),
            ["printdisabled"]
        );
        assert_eq!(
            enum_range(&values, &range("*", "no_ebook", true, false)),
            [] as [&str; 0]
        );
        assert_eq!(enum_range(&values, &range("zzz", "*", true, true)).len(), 4);
    }

    #[test]
    fn wildcards_become_anchored_regexes() {
        assert_eq!(wildcard_regex("fl*"), "fl.*");
        assert_eq!(wildcard_regex("a?b.c"), "a.b\\.c");
        assert_eq!(wildcard_regex("x(1)"), "x\\(1\\)");
    }
}
