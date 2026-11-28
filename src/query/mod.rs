//! `/select`: request parameters, query execution, sorting and `fl` projection.
//!
//! Query strings go through [`lexer`] and [`parser`] into an [`ast`], which the compiler
//! (`compile`, `edismax`, `localparams`, `join`) turns into Tantivy queries with Python
//! minisolr's semantics: the Lucene classic parser (terms, phrases with slop, `+ - ! AND
//! OR NOT`, groups, `field:(...)`, ranges incl. enum ranges, wildcards, fuzzy, `^n`,
//! `^=n`, `*:*`, `field:*`), edismax (`qf`, `pf`, `pf2`, `pf3`, `ps`, `qs`, `q.op`,
//! `mm`), local params (`{!edismax}`, `{!lucene}`, `{!terms}`, `{!field}`, `{!prefix}`,
//! `{!parent}`), `$param` dereference and `_query_:"..."`.
//!
//! Scores are rescored by function queries (edismax `boost=`, the request `boost` outside
//! edismax, `bf`; see [`crate::function`]) and ordered by `sort` (score, fields, functions,
//! `random_*`; see `sort`); either one makes the search score every hit before taking the
//! page.
//!
//! `fl` takes names, globs, `score`, `alias:field`, `[child]` and `alias:[subquery]`, as
//! Python minisolr's `project`.
//!
//! Facets (`facet.field`, `facet.query`, the JSON Facet API) are in `facet`; the JSON
//! Request API body is turned into parameters by [`json_request_params`]. `cursorMark`
//! pages as Python does: `*` starts at 0, `o:<n>` at offset `n`, and `nextCursorMark`
//! is `o:<start + returned>` (the cursor itself once a page comes back empty); no sort
//! on the uniqueKey is required, as ties already break in index order. Not yet: grouping.

pub mod ast;
mod compile;
mod edismax;
mod facet;
mod highlight;
mod join;
pub mod lexer;
pub mod localparams;
pub mod parser;
mod pyre;
mod sort;

use serde_json::{Map, Value};
use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::{DocAddress, Searcher};

use crate::Error;
use crate::function::Rescore;
use crate::index::Index;
use crate::schema::glob_match;
use compile::Compiler;

/// Request parameters; a name can repeat.
#[derive(Debug, Default, Clone)]
pub struct Params(Vec<(String, String)>);

impl Params {
    /// Builds params from name/value pairs.
    #[must_use]
    pub fn new(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    /// The last value of `name` (Python's `dict(multi)` keeps the last).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every value of `name`.
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The pairs in request order.
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }

    /// The pairs as a JSON object (last value wins), for the response header.
    #[must_use]
    pub fn to_json(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect::<Map<_, _>>(),
        )
    }
}

/// The answer to one `/select`.
#[derive(Debug)]
pub struct Selection {
    /// Total matches.
    pub num_found: usize,
    /// Offset of the first returned document.
    pub start: usize,
    /// The returned documents, after `fl`.
    pub docs: Vec<Value>,
    /// `nextCursorMark`, when the request has a `cursorMark`.
    pub next_cursor: Option<String>,
    /// `facet_counts`, when the request asks for classic facets.
    pub facet_counts: Option<Value>,
    /// `facets`, the JSON Facet API answer, for a JSON request with a `facet`.
    pub facets: Option<Value>,
    /// `highlighting`, for `hl=true`.
    pub highlighting: Option<Value>,
    /// `spellcheck`, for `spellcheck=true`.
    pub spellcheck: Option<Value>,
}

impl Selection {
    /// An empty answer.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            num_found: 0,
            start: 0,
            docs: Vec::new(),
            next_cursor: None,
            facet_counts: None,
            facets: None,
            highlighting: None,
            spellcheck: None,
        }
    }
}

fn unsupported<T>(what: impl Into<String>) -> Result<T, Error> {
    Err(Error::Unsupported(what.into()))
}

/// Runs a `/select` request.
///
/// # Errors
/// [`Error::Unsupported`] for features outside the supported subset; other variants for
/// index failures.
pub fn select(store: &Index, params: &Params) -> Result<Selection, Error> {
    execute(store, params, false, None)
}

/// Runs a JSON Request API request whose body became `params` (see
/// [`json_request_params`]); `facet` is the body's `facet`, answered in
/// [`Selection::facets`].
///
/// # Errors
/// As [`select`], plus malformed JSON facets.
pub fn select_json(
    store: &Index,
    params: &Params,
    facet: Option<&Value>,
) -> Result<Selection, Error> {
    execute(store, params, false, facet)
}

/// The parameters of a JSON Request API body, as Python's `select_json`: the URL
/// parameters, then each `params` entry (arrays repeat the name, values as Python's
/// `str()`), `query` as `q` (`*:*` when it is not a string), each `filter` as `fq`,
/// `limit` as `rows`, `offset` as `start`, `sort`, and `fields` as `fl` (an array is
/// joined with commas).
///
/// # Errors
/// A body that is not a JSON object.
pub fn json_request_params(url: &[(String, String)], body: &Value) -> Result<Params, Error> {
    let body = body
        .as_object()
        .ok_or_else(|| Error::Other("the JSON request body is not an object".into()))?;
    let mut pairs = url.to_vec();
    if let Some(Value::Object(params)) = body.get("params") {
        for (k, v) in params {
            pairs.extend(crate::index::values(v).map(|x| (k.clone(), py_str(x))));
        }
    }
    if let Some(q) = body.get("query") {
        pairs.push(("q".into(), q.as_str().unwrap_or("*:*").to_owned()));
    }
    if let Some(filter) = body.get("filter") {
        pairs.extend(crate::index::values(filter).map(|f| ("fq".into(), py_str(f))));
    }
    for (src, dst) in [("limit", "rows"), ("offset", "start"), ("sort", "sort")] {
        if let Some(v) = body.get(src) {
            pairs.push((dst.into(), py_str(v)));
        }
    }
    if let Some(fields) = body.get("fields") {
        let fl = match fields {
            Value::String(s) => s.clone(),
            other => crate::index::values(other)
                .map(py_str)
                .collect::<Vec<_>>()
                .join(","),
        };
        pairs.push(("fl".into(), fl));
    }
    Ok(Params::new(pairs))
}

/// Runs an `/export` request: a `/select` that returns every hit from `start` on, as
/// Python minisolr does (`rows` is ignored).
///
/// # Errors
/// As [`select`].
pub fn export(store: &Index, params: &Params) -> Result<Selection, Error> {
    execute(store, params, true, None)
}

/// The rows matching `q`, parsed as Python's update handler does for delete-by-query:
/// the Lucene parser with no request parameters (no `df`, `q.op=OR`).
///
/// # Errors
/// As [`select`].
pub fn matching_rows(store: &Index, q: &str) -> Result<Vec<usize>, Error> {
    let params = Params::default();
    let mut compiler = Compiler::new(store, &params);
    let query = compiler.run_query(q, &[])?;
    let searcher = store.reader.searcher();
    searcher
        .search(&*query, &DocSetCollector)?
        .into_iter()
        .map(|addr| ord_of(&searcher, addr))
        .collect()
}

/// What separates `fl` items in a top-level request (subqueries split on commas only).
const FL_SEPARATORS: &[char] = &[',', ' ', '\t', '\n', '\r'];

/// A top-level request: the search, then `cursorMark`, classic facets and JSON facets
/// over every hit.
fn execute(
    store: &Index,
    params: &Params,
    export: bool,
    json_facet: Option<&Value>,
) -> Result<Selection, Error> {
    if params.get("json.facet").is_some_and(|v| v != "false") {
        return unsupported("parameter json.facet");
    }
    let cursor = params.get("cursorMark").filter(|c| !c.is_empty());
    let start = match cursor {
        None => number(params, "start", 0)?,
        Some("*") => 0,
        Some(c) => c
            .split(':')
            .nth(1)
            .and_then(|n| n.trim().parse().ok())
            .ok_or_else(|| Error::Other(format!("bad cursorMark: {c}")))?,
    };
    let rows = if export {
        store.docs.len().max(1)
    } else {
        number(params, "rows", 10)?
    };
    let facets_wanted = facet::wanted(params);
    let found = search(
        store,
        params,
        start,
        rows,
        facets_wanted || json_facet.is_some(),
    )?;
    let next_cursor = cursor.map(|c| {
        if found.page.is_empty() {
            c.to_owned()
        } else {
            format!("o:{}", start + found.page.len())
        }
    });
    let fl = FieldList::parse(params.get("fl").unwrap_or("*").trim(), FL_SEPARATORS);
    let page_ords: Vec<usize> = found.page.iter().map(|h| h.0).collect();
    let docs = project_page(store, found.page, &fl, params)?;
    let highlighting = if params.get("hl") == Some("true") {
        Some(highlight::highlighting(store, &page_ords, params)?)
    } else {
        None
    };
    let spellcheck = if params.get("spellcheck") == Some("true") {
        Some(highlight::spellcheck(store, params)?)
    } else {
        None
    };
    if params.get("mlt") == Some("true") {
        eprintln!("warning: UNSUPPORTED (ignored): mlt=true (MoreLikeThis) is not implemented");
    }
    let facet_counts = if facets_wanted {
        Some(facet::facet_counts(store, &found.all, params)?)
    } else {
        None
    };
    let facets = json_facet
        .map(|spec| facet::json_facets(store, &found.all, spec))
        .transpose()?;
    Ok(Selection {
        num_found: found.total,
        start,
        docs,
        next_cursor,
        facet_counts,
        facets,
        highlighting,
        spellcheck,
    })
}

/// A nested request (`[subquery]`): the search and the page only, as Python's
/// `subquery`, which ignores cursors and facets.
fn run(store: &Index, params: &Params, fl_seps: &[char]) -> Result<Selection, Error> {
    let start = number(params, "start", 0)?;
    let rows = number(params, "rows", 10)?;
    let found = search(store, params, start, rows, false)?;
    let fl = FieldList::parse(params.get("fl").unwrap_or("*").trim(), fl_seps);
    Ok(Selection {
        num_found: found.total,
        start,
        docs: project_page(store, found.page, &fl, params)?,
        ..Selection::empty()
    })
}

fn project_page(
    store: &Index,
    page: Vec<sort::Hit>,
    fl: &FieldList<'_>,
    params: &Params,
) -> Result<Vec<Value>, Error> {
    page.into_iter()
        .map(|(ord, score)| project(store, ord, score, fl, params))
        .collect()
}

/// The hits of one search.
struct Found {
    total: usize,
    /// The requested page, in order.
    page: Vec<sort::Hit>,
    /// The `ord` of every hit, when asked for (facets need the whole result set).
    all: Vec<usize>,
}

fn search(
    store: &Index,
    params: &Params,
    start: usize,
    rows: usize,
    want_all: bool,
) -> Result<Found, Error> {
    if params.get("group").is_some_and(|v| v != "false") {
        return unsupported("parameter group");
    }
    let fqs: Vec<&str> = params.all("fq").collect();
    let mut compiler = Compiler::new(store, params);
    let query = compiler.run_query(params.get("q").unwrap_or("*:*"), &fqs)?;
    let boosts = std::mem::take(&mut compiler.boosts);
    // The request `boost` applies only outside edismax, where `boost` is edismax's own.
    let request_boost = params
        .get("boost")
        .filter(|b| !b.is_empty() && !matches!(params.get("defType"), Some("edismax" | "dismax")));
    let rescore = Rescore::new(
        boosts.iter().map(String::as_str).chain(request_boost),
        params.get("bf").filter(|b| !b.is_empty()),
        params,
    );
    let sort_keys = params
        .get("sort")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|spec| sort::parse_sort(store, spec, params))
        .transpose()?;
    let searcher = store.reader.searcher();
    let (num_found, page, all) = if sort_keys.is_none() && rescore.is_empty() && !want_all {
        let (count, top) = if rows == 0 {
            (searcher.search(&*query, &Count)?, Vec::new())
        } else {
            searcher.search(
                &*query,
                &(
                    Count,
                    TopDocs::with_limit(rows).and_offset(start).order_by_score(),
                ),
            )?
        };
        let page = top
            .into_iter()
            .map(|(score, addr)| Ok((ord_of(&searcher, addr)?, f64::from(score))))
            .collect::<Result<Vec<sort::Hit>, Error>>()?;
        (count, page, Vec::new())
    } else {
        // A function or a sort can move any hit into the page, so every hit is scored.
        let limit = usize::try_from(searcher.num_docs())
            .unwrap_or(usize::MAX)
            .max(1);
        let top = searcher.search(&*query, &TopDocs::with_limit(limit).order_by_score())?;
        let mut hits = top
            .into_iter()
            .map(|(score, addr)| {
                let ord = ord_of(&searcher, addr)?;
                Ok((ord, rescore.apply(store.row(ord), f64::from(score))))
            })
            .collect::<Result<Vec<sort::Hit>, Error>>()?;
        match &sort_keys {
            Some(keys) => sort::apply(store, keys, &mut hits),
            None => hits.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            }),
        }
        let found = hits.len();
        let all = if want_all {
            hits.iter().map(|h| h.0).collect()
        } else {
            Vec::new()
        };
        (
            found,
            hits.into_iter().skip(start).take(rows).collect(),
            all,
        )
    };
    Ok(Found {
        total: num_found,
        page,
        all,
    })
}

fn number(params: &Params, name: &str, default: usize) -> Result<usize, Error> {
    params.get(name).map_or(Ok(default), |v| {
        v.trim()
            .parse()
            .map_err(|_| Error::Other(format!("bad {name}: {v}")))
    })
}

pub(crate) fn ord_of(searcher: &Searcher, addr: DocAddress) -> Result<usize, Error> {
    let column = searcher
        .segment_reader(addr.segment_ord)
        .fast_fields()
        .u64("ord")?;
    column
        .first(addr.doc_id)
        .and_then(|o| usize::try_from(o).ok())
        .ok_or_else(|| Error::Other("document without ord".into()))
}

/// The parsed `fl`: whether `*` was asked for (or `fl` was empty), the items in order
/// (for `score`, `[child]`, `alias:[subquery]` and `alias:field`) and the name patterns.
pub(crate) struct FieldList<'a> {
    star: bool,
    items: Vec<&'a str>,
    patterns: Vec<&'a str>,
}

impl<'a> FieldList<'a> {
    /// Splits `fl` on runs of `seps` outside `[...]`, as Python's `re.split`.
    pub(crate) fn parse(fl: &'a str, seps: &[char]) -> Self {
        let mut items = Vec::new();
        let mut depth = 0usize;
        let mut begin = 0;
        for (i, c) in fl.char_indices() {
            match c {
                '[' => depth += 1,
                ']' => depth = depth.saturating_sub(1),
                c if depth == 0 && seps.contains(&c) => {
                    items.extend(fl.get(begin..i).filter(|s| !s.is_empty()));
                    begin = i + c.len_utf8();
                }
                _ => {}
            }
        }
        items.extend(fl.get(begin..).filter(|s| !s.is_empty()));
        Self {
            star: items.is_empty() || items.contains(&"*"),
            patterns: items
                .iter()
                .copied()
                .filter(|p| *p != "*" && !p.contains(':') && !p.starts_with('['))
                .collect(),
            items,
        }
    }
}

/// A returned document, as Python's `project`: the copyField-expanded row, minus block
/// join internals and names outside the schema; a field is kept when an `fl` pattern
/// names it or when `fl` has `*` and the field is returned in `*`. Single values of
/// multiValued fields become one-element arrays. Then the `fl` items in order: `score`,
/// `[child]` (the rows sharing this row's key as `_root_`, as `_childDocuments_`),
/// `alias:[subquery]` and `alias:field`.
pub(crate) fn project(
    store: &Index,
    ord: usize,
    hit_score: f64,
    fl: &FieldList<'_>,
    params: &Params,
) -> Result<Value, Error> {
    let Value::Object(map) = store.row(ord) else {
        return Ok(store.row(ord).clone());
    };
    let row = store.schema.effective(map);
    let mut out = Map::new();
    for (name, value) in row.iter() {
        if matches!(name.as_str(), "_root_" | "_nest_path_") {
            continue;
        }
        let Some(f) = store.schema.get(name) else {
            continue;
        };
        let explicit = fl.patterns.iter().any(|p| glob_match(p, name));
        if explicit || (fl.star && f.returned_in_star()) {
            out.insert(name.clone(), normalize(store, name, value));
        }
    }
    for item in &fl.items {
        if *item == "score" {
            out.insert("score".into(), Value::from(hit_score));
        } else if *item == "[child]" {
            out.insert("_childDocuments_".into(), children(store, &row));
        } else if let Some(alias) = item.strip_suffix(":[subquery]") {
            let alias = alias.split(':').next().unwrap_or(alias);
            out.insert(alias.to_owned(), subquery(store, &row, alias, params)?);
        } else if let Some((alias, src)) = item.split_once(':')
            && !item.starts_with('[')
            && let Some(value) = row.get(src)
        {
            out.insert(alias.to_owned(), normalize(store, src, value));
        }
    }
    Ok(Value::Object(out))
}

fn normalize(store: &Index, name: &str, value: &Value) -> Value {
    match value {
        Value::Array(_) => value.clone(),
        single if store.schema.get(name).is_some_and(|f| f.multi) => {
            Value::Array(vec![single.clone()])
        }
        single => single.clone(),
    }
}

/// `[child]`: the copyField-expanded rows whose `_root_` is this row's key (other than
/// the row itself), in store order, without `_root_`.
fn children(store: &Index, row: &Map<String, Value>) -> Value {
    let uk = store.schema.unique_key.as_str();
    let key = row.get(uk);
    let kids = store
        .docs
        .iter()
        .filter_map(|d| d.as_object())
        .filter(|d| d.get("_root_") == key && d.get(uk) != key)
        .map(|d| {
            let mut kid = store.schema.effective(d).into_owned();
            kid.remove("_root_");
            Value::Object(kid)
        })
        .collect();
    Value::Array(kids)
}

/// `alias:[subquery]`, as Python's `subquery`: the request parameters prefixed with
/// `alias.` (prefix removed) plus `row.<field>` for every field of the parent row, run
/// as a nested `/select` (default `rows` 10, `fl` `*` split on commas only).
fn subquery(
    store: &Index,
    parent: &Map<String, Value>,
    alias: &str,
    params: &Params,
) -> Result<Value, Error> {
    let prefix = format!("{alias}.");
    let mut pairs: Vec<(String, String)> = params
        .pairs()
        .iter()
        .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|k| (k.to_owned(), v.clone())))
        .collect();
    for (name, value) in parent {
        if matches!(name.as_str(), "_root_" | "_nest_path_") {
            continue;
        }
        let text = match value {
            Value::Array(items) => items.iter().map(py_str).collect::<Vec<_>>().join(","),
            Value::Bool(b) => b.to_string(),
            other => py_str(other),
        };
        pairs.push((format!("row.{name}"), text));
    }
    let sub = Params::new(pairs);
    let sel = run(store, &sub, &[','])?;
    Ok(serde_json::json!({
        "numFound": sel.num_found,
        "start": sel.start,
        "numFoundExact": true,
        "docs": sel.docs,
    }))
}

/// Python's `str()` of a JSON value: `True`/`False`, `None`, strings as is, integers in
/// decimal and floats as Python's `repr` writes them.
#[must_use]
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        Value::Number(n) if n.is_f64() => n.as_f64().map_or_else(|| n.to_string(), py_float),
        other => other.to_string(),
    }
}

/// Python's `repr` of a float: the shortest round-trip digits, positional with at
/// least one decimal for exponents from -4 to 15, otherwise `1.5e+16` / `1e-05`.
fn py_float(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "nan".into()
        } else if x > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let sci = format!("{x:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..16).contains(&exp) {
        let plain = format!("{x}");
        if plain.contains('.') {
            plain
        } else {
            format!("{plain}.0")
        }
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(pairs: &[(&str, &str)]) -> Params {
        Params::new(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn store() -> Index {
        let conf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/conf");
        let schema = crate::schema::Schema::load(&conf).expect("test schema");
        let docs = vec![
            json!({"key": "/works/OL1W", "_root_": "/works/OL1W", "type": "work", "title": "Alice in Wonderland", "author_name": ["Lewis Carroll"], "edition_count": 5, "ebook_access": "borrowable", "ia": ["alice00"], "first_publish_year": 1865}),
            json!({"key": "/works/OL2W", "_root_": "/works/OL2W", "type": "work", "title": "Through the Looking-Glass", "author_name": ["Lewis Carroll"], "edition_count": 9, "ebook_access": "no_ebook", "first_publish_year": 1871}),
            json!({"key": "/works/OL3W", "_root_": "/works/OL3W", "type": "work", "title": "Tom Sawyer", "author_name": ["Mark Twain"], "edition_count": 20, "ebook_access": "public", "ia": ["tom00"], "first_publish_year": 1876}),
            json!({"key": "/authors/OL9A", "type": "author", "name": "Lewis Carroll", "rank_i": 4, "subject": "Logic"}),
            json!({"key": "/books/OL1M", "_root_": "/works/OL1W", "type": "edition", "title": "Alice", "language": ["eng"], "publisher": ["Penguin"], "ebook_access": "borrowable"}),
            json!({"key": "/books/OL2M", "_root_": "/works/OL3W", "type": "edition", "title": "Tom Sawyer", "language": ["fre"], "publisher": ["Gallimard"], "ebook_access": "public"}),
        ];
        Index::from_docs(
            std::sync::Arc::new(schema),
            docs.into_iter().map(std::sync::Arc::new).collect(),
            0,
        )
        .expect("index builds")
    }

    fn count(store: &Index, pairs: &[(&str, &str)]) -> usize {
        match select(store, &params(pairs)) {
            Ok(sel) => sel.num_found,
            Err(e) => panic!("{pairs:?}: {e}"),
        }
    }

    fn keys(store: &Index, pairs: &[(&str, &str)]) -> Vec<String> {
        let sel = select(store, &params(pairs)).expect("select");
        sel.docs
            .iter()
            .filter_map(|d| d.get("key").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    #[test]
    fn local_params_wrapper_resolves_v_parameter() {
        let p = [
            (
                "q",
                "{!edismax q.op=\"AND\" qf=\"title^2 author_name\" v=$uq}",
            ),
            ("uq", "alice carroll"),
        ];
        assert_eq!(count(&store(), &p), 1);
    }

    #[test]
    fn field_clause_and_fq() {
        let s = store();
        let p = [
            ("q", "author_name:carroll"),
            ("fq", "type:work"),
            ("fl", "key"),
        ];
        assert_eq!(count(&s, &p), 2);
        assert!(keys(&s, &p).iter().all(|k| k.starts_with("/works/")));
    }

    #[test]
    fn numeric_sort_and_paging() {
        let s = store();
        let p = [
            ("q", "*:*"),
            ("fq", "type:work"),
            ("sort", "edition_count desc"),
            ("rows", "1"),
            ("fl", "key"),
        ];
        let sel = select(&s, &params(&p)).expect("select");
        assert_eq!(sel.docs, vec![json!({"key": "/works/OL3W"})]);
        assert_eq!(sel.num_found, 3);
        let asc = [("q", "*:*"), ("sort", "edition_count asc"), ("fl", "key")];
        assert_eq!(
            keys(&s, &asc).last().map(String::as_str),
            Some("/books/OL2M"),
            "missing values sort last"
        );
    }

    #[test]
    fn dynamic_fields_numeric_terms_and_fl() {
        let s = store();
        let sel = select(&s, &params(&[("q", "rank_i:4"), ("fl", "*")])).expect("select");
        assert_eq!(sel.num_found, 1);
        let doc = sel.docs.first().expect("one doc");
        assert_eq!(
            doc.get("subject"),
            Some(&json!(["Logic"])),
            "multiValued is wrapped"
        );
        assert_eq!(
            doc.get("subject_facet"),
            Some(&json!(["Logic"])),
            "copyField dest is docValues"
        );
        let globbed = select(&s, &params(&[("q", "rank_i:4"), ("fl", "key,*_i")])).expect("select");
        assert_eq!(
            globbed.docs,
            vec![json!({"key": "/authors/OL9A", "rank_i": 4})]
        );
        assert_eq!(count(&s, &[("q", "other_s:x")]), 0, "known but absent");
    }

    #[test]
    fn boolean_operators_and_prefixes() {
        let s = store();
        assert_eq!(
            count(
                &s,
                &[(
                    "q",
                    "title:(alice OR sawyer) -author_name:twain AND type:work"
                )]
            ),
            2
        );
        assert_eq!(count(&s, &[("q", "+type:work -ebook_access:no_ebook")]), 2);
        assert_eq!(
            count(&s, &[("q", "-type:work")]),
            3,
            "only prohibited clauses"
        );
        assert_eq!(count(&s, &[("q", "type:list OR type:work")]), 3);
        assert_eq!(count(&s, &[("q", "type:work AND title:alice")]), 1);
        assert_eq!(count(&s, &[("q", "type:work && !title:alice")]), 2);
        assert_eq!(count(&s, &[("q", "type:work NOT title:alice")]), 2);
        assert_eq!(
            count(&s, &[("q", "type:work title:alice"), ("q.op", "AND")]),
            1
        );
        assert_eq!(
            count(&s, &[("q", "(type:work (title:tom))"), ("q.op", "AND")]),
            1
        );
    }

    #[test]
    fn ranges_numeric_enum_and_open() {
        let s = store();
        assert_eq!(count(&s, &[("q", "edition_count:[5 TO 9]")]), 2);
        assert_eq!(count(&s, &[("q", "edition_count:{5 TO 9}")]), 0);
        assert_eq!(count(&s, &[("q", "edition_count:{5 TO 9]")]), 1);
        assert_eq!(count(&s, &[("q", "edition_count:[6 TO *]")]), 2);
        assert_eq!(count(&s, &[("q", "ebook_access:[borrowable TO *]")]), 4);
        assert_eq!(
            count(
                &s,
                &[
                    ("q", "*:*"),
                    ("fq", "type:work"),
                    ("fq", "ebook_access:[borrowable TO *]")
                ]
            ),
            2
        );
        assert_eq!(count(&s, &[("q", "ebook_access:{no_ebook TO public}")]), 2);
        assert_eq!(
            count(
                &s,
                &[
                    ("q", "*:*"),
                    ("fq", "edition_count:[10 TO *] OR ebook_access:borrowable"),
                    ("fq", "type:work")
                ]
            ),
            2
        );
        assert_eq!(count(&s, &[("q", "key:[/works/OL2W TO *]")]), 2);
        assert_eq!(count(&s, &[("q", "edition_count:[x TO 3]")]), 0);
    }

    #[test]
    fn wildcards_exists_fuzzy_and_prefix() {
        let s = store();
        assert_eq!(count(&s, &[("q", "title:alic*"), ("fq", "type:work")]), 1);
        assert_eq!(count(&s, &[("q", "title:al?c*")]), 2);
        assert_eq!(count(&s, &[("q", "ia:*")]), 2);
        assert_eq!(count(&s, &[("q", "edition_count:*")]), 3);
        assert_eq!(count(&s, &[("q", "key:/works/*")]), 3);
        assert_eq!(count(&s, &[("q", "title:alise~1"), ("fq", "type:work")]), 1);
        assert_eq!(count(&s, &[("q", "key:/works/OL1X~1")]), 1);
        assert_eq!(count(&s, &[("q", "{!prefix f=key v=/works/}")]), 3);
        assert_eq!(count(&s, &[("q", "{!field f=type v=edition}")]), 2);
    }

    #[test]
    fn phrases_slop_boosts_and_escapes() {
        let s = store();
        assert_eq!(count(&s, &[("q", "title:\"alice wonderland\"~1")]), 1);
        assert_eq!(count(&s, &[("q", "title:\"alice wonderland\"")]), 0);
        assert_eq!(count(&s, &[("q", "type:work^=5 title:alice^2")]), 4);
        assert_eq!(count(&s, &[("q", r"key:\/works\/OL1W")]), 1);
        assert_eq!(count(&s, &[("q", "alice"), ("df", "title")]), 2);
        assert_eq!(count(&s, &[("q", "_query_:\"type:author\"")]), 1);
        assert_eq!(count(&s, &[("q", "*:*"), ("fq", "{!tag=x}type:author")]), 1);
    }

    #[test]
    fn terms_parser() {
        let s = store();
        assert_eq!(
            count(&s, &[("q", "{!terms f=key v=/works/OL1W,/works/OL3W}")]),
            2
        );
        assert_eq!(count(&s, &[("q", "{!terms f=edition_count v=5,20}")]), 2);
        assert_eq!(
            count(
                &s,
                &[(
                    "q",
                    "{!terms f=key separator=| v='/works/OL1W|/works/OL2W'}"
                )]
            ),
            2
        );
        assert_eq!(count(&s, &[("q", "{!terms f=title v=Alice}")]), 2);
    }

    #[test]
    fn edismax_operators_and_mm() {
        let s = store();
        let base = [
            ("defType", "edismax"),
            ("qf", "title author_name^2"),
            ("fq", "type:work"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut p = base.to_vec();
            p.extend_from_slice(extra);
            count(&s, &p)
        };
        assert_eq!(with(&[("q", "alice carroll"), ("q.op", "AND")]), 1);
        assert_eq!(with(&[("q", "alice carroll")]), 2);
        assert_eq!(with(&[("q", "alice looking sawyer"), ("mm", "2")]), 0);
        assert_eq!(with(&[("q", "alice looking sawyer"), ("mm", "1")]), 3);
        assert_eq!(with(&[("q", "alice looking carroll"), ("mm", "2")]), 2);
        assert_eq!(
            with(&[("q", "alice OR sawyer"), ("q.op", "AND")]),
            0,
            "Python: mm=100% applies to explicit OR"
        );
        assert_eq!(with(&[("q", "foo:alice")]), 0, "unknown field is text");
        assert_eq!(with(&[("q", "*:*")]), 3);
        assert_eq!(
            with(&[("q", "the"), ("q.op", "AND")]),
            1,
            "the stop list is empty"
        );
        assert_eq!(
            with(&[("q", "... alice"), ("q.op", "AND")]),
            1,
            "a clause without tokens is dropped"
        );
        assert_eq!(
            with(&[
                ("q", "tom sawyer"),
                ("q.op", "AND"),
                ("pf", "title^50"),
                ("pf2", "title^20"),
                ("pf3", "title"),
                ("boost", "sum(1,2)")
            ]),
            1
        );
    }

    #[test]
    fn parent_block_join() {
        let s = store();
        let q = "+({!edismax q.op=AND qf=\"title author_name\" v=$uw}) +(_query_:\"{!parent which=type:work v=$full filters=$editions.fq}\" OR edition_count:0)";
        let join = |uw: &'static str, ufq: &'static str| {
            keys(
                &s,
                &[
                    ("q", q),
                    ("uw", uw),
                    ("full", "({!edismax v=$ufq qf=\"title publisher\"})"),
                    ("ufq", ufq),
                    ("editions.fq", "type:edition"),
                    ("fq", "type:work"),
                    ("fl", "key,editions:[subquery]"),
                ],
            )
        };
        assert_eq!(join("*:*", "+language:eng"), ["/works/OL1W"]);
        assert_eq!(join("*:*", "+publisher:gallimard"), ["/works/OL3W"]);
        assert_eq!(join("carroll", "*:*"), ["/works/OL1W"]);
        assert_eq!(join("*:*", "+language:ger"), [] as [&str; 0]);
    }

    #[test]
    fn undefined_fields_are_bad_requests_with_solrs_messages() {
        let s = store();
        let bad = |extra: &[(&str, &str)], msg: &str| {
            let mut pairs = vec![("q", "*:*")];
            pairs.extend_from_slice(extra);
            match select(&s, &params(&pairs)) {
                Err(Error::BadRequest(m)) => assert_eq!(m, msg, "{pairs:?}"),
                other => panic!("{pairs:?}: {:?}", other.map(|r| r.num_found)),
            }
        };
        let plain = "undefined field nope";
        let quoted = "undefined field: \"nope\"";
        for q in [
            "nope:x",
            "title:tom OR nope:x",
            "title:tom -nope:x",
            "nope:x*",
            "nope:\"a b\"",
            "nope:(a b)",
            "{!lucene}nope:x",
            "{!terms f=nope}a,b",
        ] {
            bad(&[("q", q)], plain);
        }
        for q in ["nope:[1 TO 2]", "{!field f=nope}a", "{!prefix f=nope}a"] {
            bad(&[("q", q)], quoted);
        }
        bad(&[("fq", "nope:x")], plain);
        bad(&[("q", "x"), ("df", "nope")], plain);
        bad(&[("facet.field", "nope")], quoted);
        bad(&[("facet", "true"), ("facet.query", "nope:x")], plain);
        bad(
            &[("sort", "nope desc")],
            "sort param field can't be found: nope",
        );
        bad(
            &[("defType", "edismax"), ("qf", "title nope")],
            "org.apache.solr.search.SyntaxError: Query Field 'nope' is not a valid field name",
        );
        for ok in [
            &[("defType", "edismax"), ("qf", "title"), ("q", "nope:x")][..],
            &[
                ("defType", "edismax"),
                ("qf", "title"),
                ("q", "tom"),
                ("pf", "nope"),
            ],
            &[("fl", "nope")],
        ] {
            assert!(select(&s, &params(ok)).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn unsupported_syntax_is_rejected() {
        let s = store();
        for q in [
            "title:[a TO b]",
            "alice",
            "{!xyz v=1}",
            "{!terms f=x",
            "a:b^1.2.3",
            "{!lucene v=$q}",
        ] {
            let err = select(&s, &params(&[("q", q)])).expect_err(q);
            assert!(matches!(err, Error::Unsupported(_)), "{q}: {err}");
        }
        let grouped = select(&s, &params(&[("q", "*:*"), ("group", "true")]));
        assert!(matches!(grouped, Err(Error::Unsupported(_))));
    }

    #[test]
    fn request_boost_outside_edismax_and_fq_boosts_discarded() {
        let s = store();
        let works = [("q", "type:work"), ("fl", "key")];
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut p = works.to_vec();
            p.extend_from_slice(extra);
            keys(&s, &p)
        };
        assert_eq!(
            with(&[("boost", "edition_count")]),
            ["/works/OL3W", "/works/OL2W", "/works/OL1W"]
        );
        assert_eq!(
            with(&[("boost", "sub(0,edition_count)"), ("rows", "1")]),
            ["/works/OL1W"]
        );
        assert_eq!(
            with(&[("fq", "{!edismax qf=title boost=sub(0,edition_count)}*:*")]),
            with(&[]),
            "a boost inside fq does not score"
        );
        assert_eq!(
            with(&[("bf", "first_publish_year  edition_count")]),
            ["/works/OL3W", "/works/OL2W", "/works/OL1W"]
        );
    }

    #[test]
    fn multi_key_sorts_and_directions() {
        let s = store();
        assert_eq!(
            keys(
                &s,
                &[
                    ("q", "*:*"),
                    ("sort", "type asc, edition_count desc"),
                    ("fl", "key")
                ]
            ),
            [
                "/authors/OL9A",
                "/books/OL1M",
                "/books/OL2M",
                "/works/OL3W",
                "/works/OL2W",
                "/works/OL1W"
            ]
        );
        assert_eq!(
            keys(
                &s,
                &[
                    ("q", "type:work"),
                    ("sort", "score asc, key desc"),
                    ("fl", "key")
                ]
            ),
            ["/works/OL3W", "/works/OL2W", "/works/OL1W"]
        );
        assert_eq!(
            keys(
                &s,
                &[
                    ("q", "type:work"),
                    ("sort", "ebook_access DESC"),
                    ("fl", "key")
                ]
            ),
            ["/works/OL3W", "/works/OL1W", "/works/OL2W"],
            "enum order, any case direction"
        );
    }

    #[test]
    fn py_str_writes_floats_as_python() {
        for (v, want) in [
            (json!(1.0), "1.0"),
            (json!(2.5), "2.5"),
            (json!(0.0001), "0.0001"),
            (json!(0.00001), "1e-05"),
            (json!(1e16), "1e+16"),
            (json!(1.5e16), "1.5e+16"),
            (json!(123_456_789_012_345.0), "123456789012345.0"),
            (json!(-3.25), "-3.25"),
            (json!(7), "7"),
            (json!(true), "True"),
        ] {
            assert_eq!(py_str(&v), want);
        }
    }

    #[test]
    fn params_last_value_wins() {
        let p = params(&[("rows", "1"), ("rows", "2"), ("fq", "a"), ("fq", "b")]);
        assert_eq!(p.get("rows"), Some("2"));
        assert_eq!(p.all("fq").collect::<Vec<_>>(), ["a", "b"]);
    }
}
