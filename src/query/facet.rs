//! Facets over the whole result set, as Python minisolr's `facets` and `json_facets`.
//!
//! Classic parameters: `facet.field` (repeatable; a leading `{!...}` is ignored) with
//! `facet.mincount` (default 0), `facet.limit` (default 100; negative is unlimited),
//! `facet.offset`, `facet.sort` (`count`, or `index`/`false`; default `count` unless the
//! limit is 0), `facet.prefix`, each overridable per field as `f.<field>.facet.<name>`;
//! `facet.query`; `json.nl=arrarr` (`[[term, count], ...]`, otherwise Solr's flat
//! `[term, count, ...]`). A document counts once per distinct value, so copyField
//! duplicates do not double count. With a mincount of 0 or less, every value of a
//! docValues string, enum, boolean or number field in the store is listed, with zero
//! counts. `facet.range`, `facet.pivot` and `facet.interval` only warn.
//!
//! JSON Facet API (a JSON request's `facet`): `"sum(field)"` (the first number of the
//! field in each hit that has it) and `{"type": "terms", "field", "limit" (10), "offset",
//! "mincount" (1), "sort"}` (by count, or by value with `"sort": "index"`). The answer
//! starts with `count`, the number of hits. Anything else only warns.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use super::{Params, matching_rows, py_str};
use crate::Error;
use crate::index::{FacetColumn, Index, Keying};
use crate::schema::Kind;

/// Whether the request asks for classic facets.
pub(crate) fn wanted(params: &Params) -> bool {
    matches!(params.get("facet"), Some("true" | "on")) || params.get("facet.field").is_some()
}

/// Python's `int()` of a parameter.
fn int_param(value: Option<&str>, default: i64, name: &str) -> Result<i64, Error> {
    value.map_or(Ok(default), |v| {
        v.trim()
            .parse()
            .map_err(|_| Error::Other(format!("bad {name}: {v}")))
    })
}

/// Python's `int()` of a JSON value.
fn int_value(value: Option<&Value>, default: i64, name: &str) -> Result<i64, Error> {
    let bad = || Error::Other(format!("bad {name}: {value:?}"));
    match value {
        None => Ok(default),
        // Python's int() truncates a float toward zero; the string form keeps the digits.
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| {
                let x = n.as_f64()?.trunc();
                format!("{x:.0}").parse().ok()
            })
            .ok_or_else(bad),
        Some(Value::String(s)) => s.trim().parse().map_err(|_| bad()),
        Some(Value::Bool(b)) => Ok(i64::from(*b)),
        Some(_) => Err(bad()),
    }
}

/// Counts the hits per value id of `column`.
fn count(column: &FacetColumn, hits: &[usize]) -> Vec<usize> {
    let mut counts = vec![0usize; column.keys.len()];
    for ids in hits.iter().filter_map(|&ord| column.rows.get(ord)) {
        for &id in ids {
            if let Some(c) = counts.get_mut(id) {
                *c += 1;
            }
        }
    }
    counts
}

/// `items[offset:offset + limit]`, or `items[offset:]` for a negative limit.
fn page<T>(items: Vec<T>, offset: i64, limit: i64) -> Vec<T> {
    let off = usize::try_from(offset).unwrap_or(0);
    let rest = items.into_iter().skip(off);
    match usize::try_from(limit) {
        Ok(n) => rest.take(n).collect(),
        Err(_) => rest.collect(),
    }
}

fn as_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The `facet_counts` section of a classic facet request over the matching rows.
pub(crate) fn facet_counts(store: &Index, hits: &[usize], params: &Params) -> Result<Value, Error> {
    let arrarr = params.get("json.nl") == Some("arrarr");
    let mut fields = Map::new();
    for raw in params.all("facet.field") {
        let field = strip_local_params(raw);
        let opt = |name: &str| {
            params
                .get(&format!("f.{field}.facet.{name}"))
                .or_else(|| params.get(&format!("facet.{name}")))
        };
        let mincount = int_param(opt("mincount"), 0, "facet.mincount")?;
        let limit = int_param(opt("limit"), 100, "facet.limit")?;
        let offset = int_param(opt("offset"), 0, "facet.offset")?;
        let by_index = matches!(
            opt("sort").unwrap_or(if limit == 0 { "index" } else { "count" }),
            "index" | "false"
        );
        let prefix = opt("prefix").filter(|p| !p.is_empty());
        let zero_terms = mincount <= 0
            && store.schema.get(field).is_some_and(|f| {
                f.doc_values
                    && matches!(
                        f.kind,
                        Kind::Str | Kind::Enum | Kind::Bool | Kind::Int | Kind::Float
                    )
            });
        let column = store.facet_column(field, Keying::Text);
        let mut items: Vec<(&str, usize)> = column
            .keys
            .iter()
            .zip(count(&column, hits))
            .filter(|(k, c)| {
                (*c > 0 || zero_terms)
                    && as_i64(*c) >= mincount
                    && prefix.is_none_or(|p| k.starts_with(p))
            })
            .map(|(k, c)| (k.as_str(), c))
            .collect();
        if by_index {
            items.sort_by(|a, b| a.0.cmp(b.0));
        } else {
            items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        }
        let items = page(items, offset, limit);
        let out: Vec<Value> = if arrarr {
            items.into_iter().map(|(k, c)| json!([k, c])).collect()
        } else {
            items
                .into_iter()
                .flat_map(|(k, c)| [Value::from(k), Value::from(c)])
                .collect()
        };
        fields.insert(field.to_owned(), Value::Array(out));
    }
    let mut queries = Map::new();
    let matched: HashSet<usize> = hits.iter().copied().collect();
    for q in params.all("facet.query") {
        let n = matching_rows(store, q)?
            .into_iter()
            .filter(|ord| matched.contains(ord))
            .count();
        queries.insert(q.to_owned(), Value::from(n));
    }
    for (name, _) in params.pairs() {
        if matches!(
            name.as_str(),
            "facet.range" | "facet.pivot" | "facet.interval"
        ) {
            eprintln!("warning: UNSUPPORTED (ignored): {name}");
        }
    }
    Ok(json!({
        "facet_queries": queries,
        "facet_fields": fields,
        "facet_ranges": {},
        "facet_intervals": {},
        "facet_heatmaps": {},
    }))
}

/// The field name of `facet.field` without a leading `{!...}`.
fn strip_local_params(raw: &str) -> &str {
    raw.strip_prefix("{!")
        .and_then(|rest| rest.split_once('}'))
        .map_or(raw, |(_, field)| field)
}

/// The `facets` section of a JSON request over the matching rows.
///
/// # Errors
/// A `terms` facet without a `field`, or a limit, offset or mincount that is not a
/// number.
pub(crate) fn json_facets(store: &Index, hits: &[usize], spec: &Value) -> Result<Value, Error> {
    let mut out = Map::new();
    out.insert("count".into(), Value::from(hits.len()));
    let Some(spec) = spec.as_object() else {
        eprintln!("warning: UNSUPPORTED (ignored): json.facet that is not an object");
        return Ok(Value::Object(out));
    };
    for (name, agg) in spec {
        match agg {
            Value::String(text) => match aggregation(text) {
                Some(("sum", Some(field))) => {
                    out.insert(name.clone(), sum(store, hits, field));
                }
                _ => eprintln!("warning: UNSUPPORTED (ignored): json.facet aggregation {text:?}"),
            },
            Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("terms") => {
                out.insert(name.clone(), terms(store, hits, o)?);
            }
            _ => eprintln!("warning: UNSUPPORTED (ignored): json.facet type in {name:?}"),
        }
    }
    Ok(Value::Object(out))
}

/// Python's `re.match(r"(\w+)\((\w+)?\)", text)`: the function name and its optional
/// single-word argument.
fn aggregation(text: &str) -> Option<(&str, Option<&str>)> {
    let word = |s: &str| {
        s.find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(s.len())
    };
    let n = word(text);
    let name = text.get(..n).filter(|s| !s.is_empty())?;
    let rest = text.get(n..)?.strip_prefix('(')?;
    let m = word(rest);
    let arg = rest.get(..m)?;
    rest.get(m..)?.strip_prefix(')')?;
    Some((name, (!arg.is_empty()).then_some(arg)))
}

/// `sum(field)`: the first value of `field` in each hit that has it, numbers only (others
/// count 0). `0` (an integer, as Python's empty `sum`) when no hit has the field.
fn sum(store: &Index, hits: &[usize], field: &str) -> Value {
    let mut total = 0.0;
    let mut any = false;
    for &ord in hits {
        let Some(doc) = store.row(ord).as_object() else {
            continue;
        };
        if let Some(values) = store.schema.values_of(doc, field) {
            any = true;
            total += values.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
        }
    }
    if any {
        Value::from(total)
    } else {
        Value::from(0)
    }
}

/// A `terms` facet: `{"buckets": [{"val", "count"}, ...]}`.
fn terms(store: &Index, hits: &[usize], spec: &Map<String, Value>) -> Result<Value, Error> {
    let field = spec
        .get("field")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Other("json.facet terms without a field".into()))?;
    let mincount = int_value(spec.get("mincount"), 1, "mincount")?;
    let limit = int_value(spec.get("limit"), 10, "limit")?;
    let offset = int_value(spec.get("offset"), 0, "offset")?;
    let by_index = spec.get("sort").and_then(Value::as_str) == Some("index");
    let column = store.facet_column(field, Keying::Json);
    let mut items: Vec<(String, &Value, usize)> = column
        .values
        .iter()
        .zip(count(&column, hits))
        .filter(|(_, c)| *c > 0 && as_i64(*c) >= mincount)
        .map(|(v, c)| (py_str(v), v, c))
        .collect();
    if by_index {
        items.sort_by(|a, b| a.0.cmp(&b.0));
    } else {
        items.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    }
    let buckets: Vec<Value> = page(items, offset, limit)
        .into_iter()
        .map(|(_, v, c)| json!({"val": v, "count": c}))
        .collect();
    Ok(json!({ "buckets": buckets }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Selection, json_request_params, select, select_json};
    use std::sync::Arc;

    fn store() -> Index {
        let conf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/conf");
        let schema = crate::schema::Schema::load(&conf).expect("test schema");
        let docs = vec![
            json!({"key": "/works/OL1W", "type": "work", "subject": ["Fiction", "Logic"], "subject_facet": ["Fiction"], "language": ["eng"], "has_fulltext": true, "edition_count": 5, "rank_i": 2}),
            json!({"key": "/works/OL2W", "type": "work", "subject": ["Fiction"], "language": ["eng", "fre"], "has_fulltext": false, "edition_count": 9, "rank_i": 2.5}),
            json!({"key": "/works/OL3W", "type": "work", "subject": ["Adventure"], "language": ["spa"], "has_fulltext": true, "edition_count": 1}),
            json!({"key": "/authors/OL1A", "type": "author", "language": ["ger"], "rank_i": 7}),
            json!({"key": "/books/OL1M", "type": "edition", "language": ["eng"]}),
        ];
        Index::from_docs(
            Arc::new(schema),
            docs.into_iter().map(Arc::new).collect(),
            0,
        )
        .expect("index builds")
    }

    fn run(store: &Index, pairs: &[(&str, &str)]) -> Selection {
        let p = Params::new(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        select(store, &p).expect("select")
    }

    fn fields(store: &Index, pairs: &[(&str, &str)]) -> Value {
        run(store, pairs).facet_counts.expect("facet_counts")["facet_fields"].clone()
    }

    #[test]
    fn counts_every_hit_with_zero_terms_by_default() {
        let s = store();
        let got = fields(
            &s,
            &[
                ("q", "type:work"),
                ("rows", "1"),
                ("facet", "true"),
                ("facet.field", "type"),
                ("json.nl", "arrarr"),
            ],
        );
        assert_eq!(
            got,
            json!({"type": [["work", 3], ["author", 0], ["edition", 0]]}),
            "all hits, not the page"
        );
        let flat = fields(&s, &[("q", "type:work"), ("facet.field", "type")]);
        assert_eq!(
            flat,
            json!({"type": ["work", 3, "author", 0, "edition", 0]}),
            "facet.field alone, flat by default"
        );
        let none = run(&s, &[("q", "type:work"), ("facet.limit", "5")]);
        assert!(
            none.facet_counts.is_none(),
            "no facet=true and no facet.field"
        );
    }

    #[test]
    fn mincount_limit_offset_sort_and_prefix() {
        let s = store();
        let base = [
            ("q", "*:*"),
            ("facet", "true"),
            ("facet.field", "language"),
            ("json.nl", "arrarr"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut p = base.to_vec();
            p.extend_from_slice(extra);
            fields(&s, &p)["language"].clone()
        };
        assert_eq!(with(&[("facet.mincount", "2")]), json!([["eng", 3]]));
        assert_eq!(
            with(&[("facet.limit", "2")]),
            json!([["eng", 3], ["fre", 1]]),
            "ties by term"
        );
        assert_eq!(
            with(&[
                ("facet.sort", "index"),
                ("facet.offset", "1"),
                ("facet.limit", "2")
            ]),
            json!([["fre", 1], ["ger", 1]])
        );
        assert_eq!(with(&[("facet.limit", "0")]), json!([]));
        assert_eq!(
            with(&[("facet.limit", "-1"), ("facet.offset", "3")]),
            json!([["spa", 1]])
        );
        assert_eq!(with(&[("facet.prefix", "f")]), json!([["fre", 1]]));
        assert_eq!(
            with(&[("facet.sort", "false"), ("facet.limit", "1")]),
            json!([["eng", 3]])
        );
    }

    #[test]
    fn per_field_options_local_params_and_types() {
        let s = store();
        let got = fields(
            &s,
            &[
                ("q", "type:work"),
                ("facet", "true"),
                ("facet.field", "{!ex=t}language"),
                ("facet.field", "has_fulltext"),
                ("facet.field", "edition_count"),
                ("facet.mincount", "1"),
                ("facet.limit", "5"),
                ("f.language.facet.limit", "1"),
                ("json.nl", "arrarr"),
            ],
        );
        assert_eq!(
            got,
            json!({"language": [["eng", 2]], "has_fulltext": [["true", 2], ["false", 1]], "edition_count": [["1", 1], ["5", 1], ["9", 1]]})
        );
    }

    #[test]
    fn copy_field_duplicates_count_once_per_document() {
        let s = store();
        let got = fields(
            &s,
            &[
                ("q", "type:work"),
                ("facet.field", "subject_facet"),
                ("facet.mincount", "1"),
                ("json.nl", "arrarr"),
            ],
        );
        assert_eq!(
            got,
            json!({"subject_facet": [["Fiction", 2], ["Adventure", 1], ["Logic", 1]]})
        );
    }

    #[test]
    fn facet_queries_count_within_the_hits() {
        let s = store();
        let fc = run(
            &s,
            &[
                ("q", "type:work"),
                ("facet", "true"),
                ("facet.query", "edition_count:[5 TO *]"),
                ("facet.query", "language:eng"),
            ],
        )
        .facet_counts
        .expect("facet_counts");
        assert_eq!(
            fc["facet_queries"],
            json!({"edition_count:[5 TO *]": 2, "language:eng": 2})
        );
        assert_eq!(fc["facet_fields"], json!({}));
        assert_eq!(fc["facet_ranges"], json!({}));
    }

    #[test]
    fn json_facets_sum_and_terms() {
        let s = store();
        let body = json!({
            "params": {"q": "type:work OR type:author", "fl": "key", "rows": 1},
            "facet": {
                "rank": "sum(rank_i)",
                "nothing": "sum(ratings_count_1)",
                "n": "count(*)",
                "lang": {"type": "terms", "field": "language", "limit": 2},
                "lang_index": {"type": "terms", "field": "language", "sort": "index", "offset": "1", "mincount": 1},
                "fulltext": {"type": "terms", "field": "has_fulltext"},
                "range": {"type": "range", "field": "rank_i"},
            }
        });
        let p = json_request_params(&[], &body).expect("params");
        let sel = select_json(&s, &p, body.get("facet")).expect("select");
        assert_eq!(sel.docs.len(), 1);
        let f = sel.facets.expect("facets");
        assert_eq!(
            f,
            json!({
                "count": 4,
                "rank": 11.5,
                "nothing": 0,
                "lang": {"buckets": [{"val": "eng", "count": 2}, {"val": "fre", "count": 1}]},
                "lang_index": {"buckets": [{"val": "fre", "count": 1}, {"val": "ger", "count": 1}, {"val": "spa", "count": 1}]},
                "fulltext": {"buckets": [{"val": true, "count": 2}, {"val": false, "count": 1}]},
            })
        );
        let bad = json!({"facet": {"x": {"type": "terms"}}});
        assert!(select_json(&s, &Params::default(), bad.get("facet")).is_err());
    }

    #[test]
    fn json_request_body_becomes_params() {
        let body = json!({
            "query": "title:x", "filter": ["type:work", "has_fulltext:true"], "limit": 5, "offset": 2,
            "sort": "key asc", "fields": ["key", "title"], "params": {"fq": "a:b", "x": [1, true]}
        });
        let p = json_request_params(&[("wt".into(), "json".into())], &body).expect("params");
        let pairs: Vec<(&str, &str)> = p
            .pairs()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("wt", "json"),
                ("fq", "a:b"),
                ("x", "1"),
                ("x", "True"),
                ("q", "title:x"),
                ("fq", "type:work"),
                ("fq", "has_fulltext:true"),
                ("rows", "5"),
                ("start", "2"),
                ("sort", "key asc"),
                ("fl", "key,title"),
            ]
        );
        let dsl = json_request_params(&[], &json!({"query": {"lucene": {}}, "fields": "key"}))
            .expect("params");
        assert_eq!(dsl.get("q"), Some("*:*"));
        assert_eq!(dsl.get("fl"), Some("key"));
        assert!(json_request_params(&[], &json!([1])).is_err());
    }

    #[test]
    fn cursor_mark_pages_through_every_hit() {
        let s = store();
        let page = |cursor: &str| {
            let sel = run(
                &s,
                &[
                    ("q", "*:*"),
                    ("sort", "key asc"),
                    ("rows", "2"),
                    ("fl", "key"),
                    ("cursorMark", cursor),
                ],
            );
            let keys: Vec<String> = sel
                .docs
                .iter()
                .filter_map(|d| d["key"].as_str().map(str::to_owned))
                .collect();
            (sel.start, keys, sel.next_cursor.expect("nextCursorMark"))
        };
        assert_eq!(
            page("*"),
            (
                0,
                vec!["/authors/OL1A".to_owned(), "/books/OL1M".to_owned()],
                "o:2".to_owned()
            )
        );
        assert_eq!(
            page("o:4"),
            (4, vec!["/works/OL3W".to_owned()], "o:5".to_owned())
        );
        assert_eq!(
            page("o:5"),
            (5, Vec::new(), "o:5".to_owned()),
            "an empty page keeps the cursor"
        );
        let p = Params::new(vec![
            ("q".into(), "*:*".into()),
            ("cursorMark".into(), "AoE".into()),
        ]);
        assert!(select(&s, &p).is_err());
        assert!(run(&s, &[("q", "*:*")]).next_cursor.is_none());
    }

    #[test]
    fn parses_aggregations_like_the_python_regex() {
        assert_eq!(
            aggregation("sum(readinglog_count)"),
            Some(("sum", Some("readinglog_count")))
        );
        assert_eq!(aggregation("sum()"), Some(("sum", None)));
        assert_eq!(aggregation("sum(a)trailing"), Some(("sum", Some("a"))));
        assert_eq!(aggregation("count(*)"), None);
        assert_eq!(aggregation("sum(a,b)"), None);
        assert_eq!(aggregation("(a)"), None);
    }

    #[test]
    fn strips_local_params_and_pages() {
        assert_eq!(strip_local_params("{!ex=lang}language"), "language");
        assert_eq!(strip_local_params("language"), "language");
        assert_eq!(page(vec![1, 2, 3, 4], 1, 2), [2, 3]);
        assert_eq!(page(vec![1, 2, 3, 4], 1, -1), [2, 3, 4]);
        assert_eq!(page(vec![1, 2, 3, 4], 0, 0), [] as [i32; 0]);
        assert_eq!(int_value(Some(&json!("5")), 1, "x").ok(), Some(5));
        assert_eq!(int_value(Some(&json!(2.7)), 1, "x").ok(), Some(2));
        assert!(int_value(Some(&json!([1])), 1, "x").is_err());
    }
}
