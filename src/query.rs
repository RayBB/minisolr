//! Request parsing and query execution for `/select`.
//!
//! Supported: plain `q` terms, quoted phrases and `field:term` clauses joined by AND; a
//! dismax-like match over a `qf` list (from `defType=edismax|dismax` with `qf`, or a
//! whole-query `{!edismax qf=... v=$param}` wrapper); `fq` of `field:value`; `rows`,
//! `start`, `fl`; the highlighting parameters are ignored; ordering by score or by one numeric field. Everything else (boosts,
//! `pf`, ranges, wildcards, boolean operators other than `AND`, facets) is rejected with
//! [`Error::Unsupported`]. Boost and `pf` parameters only change ranking, so they are ignored.

use serde_json::{Map, Value};
use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, EmptyQuery, Occur, PhraseQuery, Query,
    TermQuery,
};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocAddress, Searcher, Term};

use crate::Error;
use crate::schema::{Kind, glob_match};
use crate::store::{Indexed, Store};

/// Request parameters; a name can repeat.
#[derive(Debug, Default, Clone)]
pub struct Params(Vec<(String, String)>);

impl Params {
    /// Builds params from name/value pairs.
    #[must_use]
    pub fn new(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    /// The first value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
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
}

/// One clause of the user query.
#[derive(Debug, PartialEq)]
struct Clause {
    field: Option<String>,
    text: String,
    phrase: bool,
}

fn unsupported<T>(what: impl Into<String>) -> Result<T, Error> {
    Err(Error::Unsupported(what.into()))
}

/// Runs a `/select` request.
///
/// # Errors
/// [`Error::Unsupported`] for features outside the supported subset; other variants for
/// index failures.
pub fn select(store: &Store, params: &Params) -> Result<Selection, Error> {
    let rows = number(params, "rows", 10)?;
    let start = number(params, "start", 0)?;
    for unsupported_param in ["facet", "group", "cursorMark", "json.facet"] {
        if params.get(unsupported_param).is_some_and(|v| v != "false") {
            return unsupported(format!("parameter {unsupported_param}"));
        }
    }
    let query = build_query(store, params)?;
    let searcher = store.reader.searcher();
    let (num_found, ords) = match parse_sort(store, params.get("sort"))? {
        None => {
            let (count, top) = if rows == 0 {
                (searcher.search(&query, &Count)?, Vec::new())
            } else {
                searcher.search(
                    &query,
                    &(
                        Count,
                        TopDocs::with_limit(rows).and_offset(start).order_by_score(),
                    ),
                )?
            };
            let ords = top
                .into_iter()
                .map(|(_, addr)| ord_of(&searcher, addr))
                .collect::<Result<Vec<_>, _>>()?;
            (count, ords)
        }
        Some((field, descending)) => {
            let hits = searcher.search(&query, &DocSetCollector)?;
            let mut ords = hits
                .into_iter()
                .map(|addr| ord_of(&searcher, addr))
                .collect::<Result<Vec<_>, _>>()?;
            let key = |ord: &usize| {
                store
                    .docs
                    .get(*ord)
                    .and_then(|d| first_number(d.get(field.as_str())))
            };
            ords.sort_by(|a, b| {
                let ord = key(a)
                    .partial_cmp(&key(b))
                    .unwrap_or(std::cmp::Ordering::Equal);
                (if descending { ord.reverse() } else { ord }).then(a.cmp(b))
            });
            let found = ords.len();
            (found, ords.into_iter().skip(start).take(rows).collect())
        }
    };
    let fl = field_list(params.get("fl"));
    let docs = ords
        .into_iter()
        .filter_map(|o| store.docs.get(o))
        .map(|d| project(store, d, &fl))
        .collect();
    Ok(Selection {
        num_found,
        start,
        docs,
    })
}

fn number(params: &Params, name: &str, default: usize) -> Result<usize, Error> {
    params.get(name).map_or(Ok(default), |v| {
        v.trim()
            .parse()
            .map_err(|_| Error::Other(format!("bad {name}: {v}")))
    })
}

fn first_number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Array(items) => items.first().and_then(Value::as_f64),
        other => other.as_f64(),
    }
}

fn ord_of(searcher: &Searcher, addr: DocAddress) -> Result<usize, Error> {
    let column = searcher
        .segment_reader(addr.segment_ord)
        .fast_fields()
        .u64("ord")?;
    column
        .first(addr.doc_id)
        .and_then(|o| usize::try_from(o).ok())
        .ok_or_else(|| Error::Other("document without ord".into()))
}

/// `sort=<numeric field> asc|desc`, or `None` for score order.
fn parse_sort(store: &Store, sort: Option<&str>) -> Result<Option<(String, bool)>, Error> {
    let Some(sort) = sort.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let mut it = sort.split_whitespace();
    let (Some(name), Some(dir), None) = (it.next(), it.next(), it.next()) else {
        return unsupported(format!("sort {sort}"));
    };
    let descending = match dir {
        "asc" => false,
        "desc" => true,
        _ => return unsupported(format!("sort {sort}")),
    };
    if name == "score" {
        return if descending {
            Ok(None)
        } else {
            unsupported("sort score asc")
        };
    }
    match store.schema.get(name) {
        Some(f) if f.kind.is_numeric() => Ok(Some((f.name.clone(), descending))),
        _ => unsupported(format!("sort {sort}")),
    }
}

/// The parsed `fl`: whether `*` was asked for (or `fl` was empty) and the name patterns.
struct FieldList<'a> {
    star: bool,
    patterns: Vec<&'a str>,
}

fn field_list(fl: Option<&str>) -> FieldList<'_> {
    let items: Vec<&str> = fl
        .unwrap_or_default()
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .collect();
    FieldList {
        star: items.is_empty() || items.contains(&"*"),
        patterns: items
            .into_iter()
            .filter(|p| *p != "*" && !p.contains(':') && !p.starts_with('['))
            .collect(),
    }
}

/// A returned document, as Python's `project`: the copyField-expanded row, minus block
/// join internals and names outside the schema; a field is kept when an `fl` pattern
/// names it or when `fl` has `*` and the field is returned in `*`. Single values of
/// multiValued fields become one-element arrays.
fn project(store: &Store, doc: &Value, fl: &FieldList<'_>) -> Value {
    let Value::Object(map) = doc else {
        return doc.clone();
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
            let value = match value {
                Value::Array(_) => value.clone(),
                single if f.multi => Value::Array(vec![single.clone()]),
                single => single.clone(),
            };
            out.insert(name.clone(), value);
        }
    }
    Value::Object(out)
}

/// What `q` asks for after resolving a `{!edismax ...}` wrapper.
struct Resolved<'a> {
    body: std::borrow::Cow<'a, str>,
    qf: Option<&'a str>,
    q_op: Option<&'a str>,
    dismax: bool,
}

fn build_query(store: &Store, params: &Params) -> Result<Box<dyn Query>, Error> {
    let resolved = resolve(params)?;
    let qf = match (resolved.qf, resolved.dismax) {
        (Some(qf), _) => parse_qf(qf)?,
        (None, true) => return unsupported("edismax without qf"),
        (None, false) => Vec::new(),
    };
    let (clauses, explicit_and) = parse_clauses(&resolved.body)?;
    if clauses.len() > 1 && !explicit_and && resolved.q_op != Some("AND") {
        return unsupported("several clauses without q.op=AND");
    }
    let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for clause in &clauses {
        if clause.field.is_none() && qf.is_empty() {
            return unsupported("bare term without qf");
        }
        if let Some(q) = clause_query(store, clause, &qf)? {
            must.push((Occur::Must, q));
        }
    }
    for fq in params.all("fq") {
        let (filters, _) = parse_clauses(fq)?;
        let [clause] = filters.as_slice() else {
            return unsupported(format!("fq {fq}"));
        };
        if clause.field.is_none() {
            return unsupported(format!("fq {fq}"));
        }
        if let Some(q) = clause_query(store, clause, &[])? {
            must.push((Occur::Must, Box::new(ConstScoreQuery::new(q, 0.0))));
        }
    }
    Ok(match must.len() {
        0 => Box::new(AllQuery),
        _ => Box::new(BooleanQuery::new(must)),
    })
}

fn resolve(params: &Params) -> Result<Resolved<'_>, Error> {
    let q = params.get("q").unwrap_or("*:*").trim();
    let q = q
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .filter(|inner| inner.starts_with("{!"))
        .unwrap_or(q);
    let dismax = matches!(params.get("defType"), Some("edismax" | "dismax"));
    let Some(rest) = q.strip_prefix("{!") else {
        return Ok(Resolved {
            body: q.into(),
            qf: params.get("qf"),
            q_op: params.get("q.op"),
            dismax,
        });
    };
    let (kind, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if !matches!(kind, "edismax" | "dismax") {
        return unsupported(format!("local params type {kind}"));
    }
    let (local, tail) = parse_local_params(rest)?;
    let mut resolved = Resolved {
        body: tail.trim().into(),
        qf: None,
        q_op: None,
        dismax: true,
    };
    for (key, value) in local {
        match key {
            "qf" => resolved.qf = Some(value),
            "q.op" => resolved.q_op = Some(value),
            "v" => {
                resolved.body = match value.strip_prefix('$') {
                    Some(name) => params.get(name).unwrap_or_default().trim().into(),
                    None => value.into(),
                };
            }
            "pf" | "pf2" | "pf3" | "boost" | "bf" | "tie" | "ps" => {}
            other => return unsupported(format!("local param {other}")),
        }
    }
    Ok(resolved)
}

/// Parses `key=value` pairs up to the closing `}`; returns them and the text after it.
type LocalParams<'a> = (Vec<(&'a str, &'a str)>, &'a str);

fn parse_local_params(mut s: &str) -> Result<LocalParams<'_>, Error> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start();
        if let Some(tail) = s.strip_prefix('}') {
            return Ok((out, tail));
        }
        let Some((key, after)) = s.split_once('=') else {
            return unsupported("malformed local params");
        };
        let (value, tail) = if let Some(quote @ ('"' | '\'')) = after.chars().next() {
            let inner = after.get(1..).unwrap_or_default();
            inner
                .split_once(quote)
                .ok_or_else(|| Error::Unsupported("unterminated local param".into()))?
        } else {
            let end = after.find([' ', '}']).unwrap_or(after.len());
            (
                after.get(..end).unwrap_or_default(),
                after.get(end..).unwrap_or_default(),
            )
        };
        out.push((key.trim(), value));
        s = tail;
        if s.is_empty() {
            return unsupported("unterminated local params");
        }
    }
}

fn parse_qf(qf: &str) -> Result<Vec<(&str, f32)>, Error> {
    qf.split_whitespace()
        .map(|item| {
            let (name, boost) = match item.split_once('^') {
                Some((n, b)) => (
                    n,
                    b.parse()
                        .map_err(|_| Error::Unsupported(format!("qf boost {item}")))?,
                ),
                None => (item, 1.0),
            };
            Ok((name, boost))
        })
        .collect()
}

/// Splits a query into clauses. The flag says whether every join was an explicit `AND`.
fn parse_clauses(q: &str) -> Result<(Vec<Clause>, bool), Error> {
    let mut clauses = Vec::new();
    let mut explicit_and = true;
    let mut rest = q.trim();
    let mut expect_clause = true;
    while !rest.is_empty() {
        let (word, after) = next_word(rest)?;
        rest = after.trim_start();
        if word == "AND" && !expect_clause {
            expect_clause = true;
            continue;
        }
        if !expect_clause {
            explicit_and = false;
        }
        expect_clause = false;
        if word == "*:*" {
            continue;
        }
        clauses.push(parse_clause(word)?);
    }
    Ok((clauses, explicit_and))
}

/// The next whitespace-delimited word, keeping a quoted phrase (with an optional
/// `field:` prefix) together.
fn next_word(s: &str) -> Result<(&str, &str), Error> {
    let end = match s.find('"') {
        Some(open)
            if s.get(..open)
                .is_some_and(|p| !p.contains(char::is_whitespace)) =>
        {
            let after_open = s.get(open + 1..).unwrap_or_default();
            let close = after_open
                .find('"')
                .ok_or_else(|| Error::Unsupported("unterminated quote".into()))?;
            open + 1 + close + 1
        }
        _ => s.find(char::is_whitespace).unwrap_or(s.len()),
    };
    Ok((
        s.get(..end).unwrap_or_default(),
        s.get(end..).unwrap_or_default(),
    ))
}

fn parse_clause(word: &str) -> Result<Clause, Error> {
    let (field, value) = match word.split_once(':') {
        Some((name, value)) if !name.contains('"') => (Some(name.to_owned()), value),
        _ => (None, word),
    };
    let (text, phrase) = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => (inner, true),
        None => (value, false),
    };
    let bad_start = text.starts_with(['+', '-', '!', '(', ')', '[', ']', '{', '}', '<', '>']);
    if text.is_empty()
        || bad_start
        || text.contains(['*', '?', '^', '~', '\\']) && !phrase
        || matches!(text, "OR" | "NOT" | "TO")
    {
        return unsupported(format!("query syntax in {word:?}"));
    }
    Ok(Clause {
        field,
        text: text.to_owned(),
        phrase,
    })
}

/// The query for one clause: its own field, or any `qf` field with its boost.
fn clause_query(
    store: &Store,
    clause: &Clause,
    qf: &[(&str, f32)],
) -> Result<Option<Box<dyn Query>>, Error> {
    let own = clause.field.as_deref().map(|f| [(f, 1.0)]);
    let targets = own.as_ref().map_or(qf, |t| t.as_slice());
    let mut should: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let mut any_indexed = false;
    for &(name, boost) in targets {
        let Some(indexed) = store.resolve(name)? else {
            continue;
        };
        any_indexed = true;
        let Some(q) = field_query(store, indexed, clause)? else {
            continue;
        };
        let q: Box<dyn Query> = if (boost - 1.0).abs() > f32::EPSILON {
            Box::new(BoostQuery::new(q, boost))
        } else {
            q
        };
        should.push((Occur::Should, q));
    }
    Ok(match should.len() {
        // Fields the schema defines but no document has: the clause matches nothing.
        0 if !any_indexed => Some(Box::new(EmptyQuery)),
        // Analysis left no tokens (punctuation only): the clause is dropped, as in Solr.
        0 => None,
        1 => should.pop().map(|(_, q)| q),
        _ => Some(Box::new(BooleanQuery::new(should))),
    })
}

fn field_query(
    store: &Store,
    indexed: &Indexed,
    clause: &Clause,
) -> Result<Option<Box<dyn Query>>, Error> {
    let Indexed { def, handle } = indexed;
    let field = *handle;
    let term = |text: &str| Term::from_field_text(field, text);
    let exact =
        |t: Term| Some(Box::new(TermQuery::new(t, IndexRecordOption::Basic)) as Box<dyn Query>);
    Ok(match def.kind {
        Kind::Text => {
            let mut terms: Vec<Term> = store
                .analyzer
                .query(def.chain, &clause.text)
                .iter()
                .map(|t| term(t))
                .collect();
            match terms.len() {
                0 => None,
                1 => terms.pop().map(|t| {
                    Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs)) as Box<dyn Query>
                }),
                _ => Some(Box::new(PhraseQuery::new(terms))),
            }
        }
        Kind::Bool => exact(term(&clause.text.to_lowercase())),
        Kind::Int | Kind::Float => match clause.text.parse::<f64>() {
            Ok(x) => exact(Term::from_field_f64(field, x)),
            Err(_) => return unsupported(format!("non-numeric value for {}", def.name)),
        },
        Kind::Str | Kind::Enum | Kind::Date => exact(term(&clause.text)),
        Kind::SortText | Kind::Skip => {
            return unsupported(format!("query on {:?} field {}", def.kind, def.name));
        }
    })
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

    fn store() -> Store {
        let conf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/conf");
        let schema = crate::schema::Schema::load(&conf).expect("test schema");
        Store::from_docs(schema, vec![
            json!({"key": "/works/OL1W", "type": "work", "title": "Alice in Wonderland", "author_name": ["Lewis Carroll"], "edition_count": 5}),
            json!({"key": "/works/OL2W", "type": "work", "title": "Through the Looking-Glass", "author_name": ["Lewis Carroll"], "edition_count": 9}),
            json!({"key": "/authors/OL9A", "type": "author", "name": "Lewis Carroll", "rank_i": 4, "subject": "Logic"}),
        ])
        .expect("index builds")
    }

    #[test]
    fn local_params_wrapper_resolves_v_parameter() {
        let p = params(&[
            (
                "q",
                "{!edismax q.op=\"AND\" qf=\"title^2 author_name\" v=$uq}",
            ),
            ("uq", "alice carroll"),
        ]);
        let sel = select(&store(), &p).expect("select");
        assert_eq!(sel.num_found, 1);
    }

    #[test]
    fn field_clause_and_fq() {
        let p = params(&[
            ("q", "author_name:carroll"),
            ("fq", "type:work"),
            ("fl", "key"),
        ]);
        let sel = select(&store(), &p).expect("select");
        assert_eq!(sel.num_found, 2);
        assert!(sel.docs.iter().all(|d| {
            d.get("key")
                .is_some_and(|k| k.as_str().is_some_and(|k| k.starts_with("/works/")))
        }));
    }

    #[test]
    fn numeric_sort_and_paging() {
        let p = params(&[
            ("q", "*:*"),
            ("fq", "type:work"),
            ("sort", "edition_count desc"),
            ("rows", "1"),
            ("fl", "key"),
        ]);
        let sel = select(&store(), &p).expect("select");
        assert_eq!(sel.docs, vec![json!({"key": "/works/OL2W"})]);
        assert_eq!(sel.num_found, 2);
    }

    #[test]
    fn dynamic_fields_numeric_terms_and_fl() {
        let p = params(&[("q", "rank_i:4"), ("fl", "*")]);
        let sel = select(&store(), &p).expect("select");
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
        let globbed =
            select(&store(), &params(&[("q", "rank_i:4"), ("fl", "key,*_i")])).expect("select");
        assert_eq!(
            globbed.docs,
            vec![json!({"key": "/authors/OL9A", "rank_i": 4})]
        );
        let none = select(&store(), &params(&[("q", "other_s:x")])).expect("known but absent");
        assert_eq!(none.num_found, 0);
        let undefined = select(&store(), &params(&[("q", "nope:x")])).expect_err("undefined");
        assert!(matches!(undefined, Error::Unsupported(_)));
    }

    #[test]
    fn unsupported_syntax_is_rejected() {
        for q in ["title:(a OR b)", "ti*e", "-alice", "title:[1 TO 2]"] {
            let err = select(&store(), &params(&[("q", q)])).expect_err(q);
            assert!(matches!(err, Error::Unsupported(_)), "{q}: {err}");
        }
    }
}
