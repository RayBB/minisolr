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
//! Not yet: facets, grouping, cursors, and the `[subquery]` / `[child]` document
//! transformers (their `fl` items are ignored; they do not change `numFound`).

pub mod ast;
mod compile;
mod edismax;
mod join;
pub mod lexer;
pub mod localparams;
pub mod parser;
mod sort;

use serde_json::{Map, Value};
use tantivy::collector::{Count, TopDocs};
use tantivy::{DocAddress, Searcher};

use crate::Error;
use crate::function::Rescore;
use crate::schema::glob_match;
use crate::store::Store;
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
    if matches!(params.get("facet"), Some("true" | "on")) || params.get("facet.field").is_some() {
        return unsupported("facets");
    }
    for unsupported_param in ["group", "cursorMark", "json.facet"] {
        if params.get(unsupported_param).is_some_and(|v| v != "false") {
            return unsupported(format!("parameter {unsupported_param}"));
        }
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
        .map(|spec| sort::parse_sort(store, spec, params));
    let searcher = store.reader.searcher();
    let (num_found, ords) = if sort_keys.is_none() && rescore.is_empty() {
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
        let ords = top
            .into_iter()
            .map(|(_, addr)| ord_of(&searcher, addr))
            .collect::<Result<Vec<_>, _>>()?;
        (count, ords)
    } else {
        // A function or a sort can move any hit into the page, so every hit is scored.
        let all = usize::try_from(searcher.num_docs())
            .unwrap_or(usize::MAX)
            .max(1);
        let top = searcher.search(&*query, &TopDocs::with_limit(all).order_by_score())?;
        let mut hits = top
            .into_iter()
            .map(|(score, addr)| {
                let ord = ord_of(&searcher, addr)?;
                let doc = store.docs.get(ord).unwrap_or(&Value::Null);
                Ok((ord, rescore.apply(doc, f64::from(score))))
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
        (
            found,
            hits.into_iter()
                .skip(start)
                .take(rows)
                .map(|(ord, _)| ord)
                .collect(),
        )
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
            json!({"key": "/works/OL1W", "_root_": "/works/OL1W", "type": "work", "title": "Alice in Wonderland", "author_name": ["Lewis Carroll"], "edition_count": 5, "ebook_access": "borrowable", "ia": ["alice00"], "first_publish_year": 1865}),
            json!({"key": "/works/OL2W", "_root_": "/works/OL2W", "type": "work", "title": "Through the Looking-Glass", "author_name": ["Lewis Carroll"], "edition_count": 9, "ebook_access": "no_ebook", "first_publish_year": 1871}),
            json!({"key": "/works/OL3W", "_root_": "/works/OL3W", "type": "work", "title": "Tom Sawyer", "author_name": ["Mark Twain"], "edition_count": 20, "ebook_access": "public", "ia": ["tom00"], "first_publish_year": 1876}),
            json!({"key": "/authors/OL9A", "type": "author", "name": "Lewis Carroll", "rank_i": 4, "subject": "Logic"}),
            json!({"key": "/books/OL1M", "_root_": "/works/OL1W", "type": "edition", "title": "Alice", "language": ["eng"], "publisher": ["Penguin"], "ebook_access": "borrowable"}),
            json!({"key": "/books/OL2M", "_root_": "/works/OL3W", "type": "edition", "title": "Tom Sawyer", "language": ["fre"], "publisher": ["Gallimard"], "ebook_access": "public"}),
        ])
        .expect("index builds")
    }

    fn count(store: &Store, pairs: &[(&str, &str)]) -> usize {
        match select(store, &params(pairs)) {
            Ok(sel) => sel.num_found,
            Err(e) => panic!("{pairs:?}: {e}"),
        }
    }

    fn keys(store: &Store, pairs: &[(&str, &str)]) -> Vec<String> {
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
    fn unsupported_syntax_is_rejected() {
        let s = store();
        for q in [
            "title:[a TO b]",
            "nope:x",
            "alice",
            "{!xyz v=1}",
            "{!terms f=x",
            "a:b^1.2.3",
            "{!lucene v=$q}",
        ] {
            let err = select(&s, &params(&[("q", q)])).expect_err(q);
            assert!(matches!(err, Error::Unsupported(_)), "{q}: {err}");
        }
        let facets = select(&s, &params(&[("q", "*:*"), ("facet.field", "type")]));
        assert!(matches!(facets, Err(Error::Unsupported(_))));
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
    fn params_last_value_wins() {
        let p = params(&[("rows", "1"), ("rows", "2"), ("fq", "a"), ("fq", "b")]);
        assert_eq!(p.get("rows"), Some("2"));
        assert_eq!(p.all("fq").collect::<Vec<_>>(), ["a", "b"]);
    }
}
