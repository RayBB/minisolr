//! `sort=`: multi-key sorts on score, fields, functions and `random_*`, as Python
//! minisolr's `apply_sort` does them.
//!
//! The hits start in index order; each key, last to first, is a stable sort, so earlier
//! keys win and index order breaks the remaining ties. Documents without a value for a key
//! go after those with one in both directions. Values come from the stored row (copyField
//! targets are not filled in), as in Python.

use std::cmp::Ordering;

use serde_json::Value;

use crate::Error;
use crate::analysis::fold;
use crate::function::ast::Expr;
use crate::function::parse::parse as parse_function;
use crate::index::Index;
use crate::md5;
use crate::query::Params;
use crate::schema::Kind;

/// A hit: the document's `ord` and its final score.
pub(super) type Hit = (usize, f64);

/// One `expr asc|desc` item of `sort`.
#[derive(Debug)]
pub(super) struct SortKey<'a> {
    by: By<'a>,
    descending: bool,
}

#[derive(Debug)]
enum By<'a> {
    Score,
    /// `random_<seed>`: the MD5 of the whole name and the document key.
    Random(&'a str),
    Function(Expr<'a>),
    Field(&'a str, FieldOrder<'a>),
}

/// How a string field's values compare.
#[derive(Debug)]
enum FieldOrder<'a> {
    /// Position in `enumsConfig.xml` (-1 for values not listed).
    Enum(&'a [String]),
    /// ICU collation approximated by folding.
    Collated,
    /// Plain code point order.
    Plain,
}

#[derive(Debug, PartialEq)]
enum SortValue {
    Number(f64),
    Text(String),
}

impl SortValue {
    fn compare(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            (Self::Number(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Number(_)) => Ordering::Greater,
        }
    }
}

/// Parses `sort`: items split on commas outside parentheses, each `expr direction` split
/// at the last space; only `desc` (any case) is descending. An item without a space is
/// Python's `rpartition` result: an empty expression, which no document has.
///
/// # Errors
/// [`Error::BadRequest`] for a field the schema does not define, as Solr.
pub(super) fn parse_sort<'a>(
    store: &'a Index,
    spec: &'a str,
    params: &'a Params,
) -> Result<Vec<SortKey<'a>>, Error> {
    split_top_level(spec)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            let (expr, direction) = item.rsplit_once(' ').unwrap_or(("", item));
            Ok(SortKey {
                by: by(store, expr.trim(), params)?,
                descending: direction.eq_ignore_ascii_case("desc"),
            })
        })
        .collect()
}

fn by<'a>(store: &'a Index, expr: &'a str, params: &'a Params) -> Result<By<'a>, Error> {
    Ok(if expr == "score" {
        By::Score
    } else if expr.starts_with("random_") {
        By::Random(expr)
    } else if expr.contains('(') {
        By::Function(parse_function(expr, params))
    } else {
        let order = match store.schema.get(expr) {
            None if expr.is_empty() => FieldOrder::Plain,
            None => {
                return Err(Error::BadRequest(format!(
                    "sort param field can't be found: {expr}"
                )));
            }
            Some(f) => match f.kind {
                Kind::Enum => FieldOrder::Enum(
                    store
                        .schema
                        .field_type(&f)
                        .map_or(&[][..], |t| t.enum_values.as_slice()),
                ),
                Kind::SortText => FieldOrder::Collated,
                _ => FieldOrder::Plain,
            },
        };
        By::Field(expr, order)
    })
}

/// Splits on commas at parenthesis depth 0. (Python's regex split also splits inside a
/// nested call such as `def(a,max(b,c))`; none of the captured sorts nest.)
fn split_top_level(spec: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    spec.split(move |c| {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        c == ',' && depth == 0
    })
}

/// Orders `hits` by `keys`.
pub(super) fn apply(store: &Index, keys: &[SortKey<'_>], hits: &mut Vec<Hit>) {
    hits.sort_by_key(|h| h.0);
    for key in keys.iter().rev() {
        let mut have = Vec::with_capacity(hits.len());
        let mut missing = Vec::new();
        for hit in hits.drain(..) {
            let doc = store.row(hit.0);
            match key.value(doc, hit.1) {
                Some(v) => have.push((hit, v)),
                None => missing.push(hit),
            }
        }
        have.sort_by(|a, b| {
            let o = a.1.compare(&b.1);
            if key.descending { o.reverse() } else { o }
        });
        hits.extend(have.into_iter().map(|(hit, _)| hit));
        hits.extend(missing);
    }
}

impl SortKey<'_> {
    fn value(&self, doc: &Value, score: f64) -> Option<SortValue> {
        match &self.by {
            By::Score => Some(SortValue::Number(score)),
            By::Random(seed) => {
                let key = match doc.get("key") {
                    Some(Value::String(s)) => s.clone(),
                    None | Some(Value::Null) => "None".to_owned(),
                    Some(other) => other.to_string(),
                };
                let n = md5::leading_u32(format!("{seed}{key}").as_bytes());
                Some(SortValue::Number(f64::from(n)))
            }
            By::Function(expr) => Some(SortValue::Number(expr.eval(doc, score))),
            By::Field(name, order) => field_value(doc, name, order),
        }
    }
}

fn field_value(doc: &Value, name: &str, order: &FieldOrder<'_>) -> Option<SortValue> {
    let value = match doc.get(name)? {
        Value::Array(items) => items.first()?,
        single => single,
    };
    match value {
        Value::Number(n) => n.as_f64().map(SortValue::Number),
        Value::Bool(b) => Some(SortValue::Number(f64::from(u8::from(*b)))),
        Value::String(s) => Some(match order {
            FieldOrder::Enum(values) => {
                let position = values.iter().position(|v| v == s);
                SortValue::Number(
                    position.map_or(-1.0, |p| f64::from(u32::try_from(p).unwrap_or(u32::MAX))),
                )
            }
            FieldOrder::Collated => SortValue::Text(fold(s).trim().to_owned()),
            FieldOrder::Plain => SortValue::Text(s.clone()),
        }),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_items_outside_parentheses() {
        let items: Vec<&str> = split_top_level("def(trending_z_score, 0) desc, key asc").collect();
        assert_eq!(items, ["def(trending_z_score, 0) desc", " key asc"]);
        let nested: Vec<&str> = split_top_level("max(a,min(b,c)) asc,x desc").collect();
        assert_eq!(nested, ["max(a,min(b,c)) asc", "x desc"]);
    }

    #[test]
    fn values_compare_numbers_before_text() {
        assert_eq!(
            SortValue::Number(2.0).compare(&SortValue::Number(10.0)),
            Ordering::Less
        );
        assert_eq!(
            SortValue::Text("b".into()).compare(&SortValue::Text("a".into())),
            Ordering::Greater
        );
        assert_eq!(
            SortValue::Number(9.0).compare(&SortValue::Text("a".into())),
            Ordering::Less
        );
        assert_eq!(
            SortValue::Number(f64::NAN).compare(&SortValue::Number(1.0)),
            Ordering::Equal
        );
    }

    #[test]
    fn field_values() {
        let doc = serde_json::json!({"n": [3, 1], "s": "Éclair", "b": true, "e": [], "z": null, "ebook_access": "public"});
        let enums = [
            "no_ebook".to_owned(),
            "borrowable".to_owned(),
            "public".to_owned(),
        ];
        assert_eq!(
            field_value(&doc, "n", &FieldOrder::Plain),
            Some(SortValue::Number(3.0))
        );
        assert_eq!(
            field_value(&doc, "b", &FieldOrder::Plain),
            Some(SortValue::Number(1.0))
        );
        assert_eq!(
            field_value(&doc, "s", &FieldOrder::Collated),
            Some(SortValue::Text("eclair".into()))
        );
        assert_eq!(
            field_value(&doc, "s", &FieldOrder::Plain),
            Some(SortValue::Text("Éclair".into()))
        );
        assert_eq!(
            field_value(&doc, "ebook_access", &FieldOrder::Enum(&enums)),
            Some(SortValue::Number(2.0))
        );
        assert_eq!(
            field_value(&doc, "s", &FieldOrder::Enum(&enums)),
            Some(SortValue::Number(-1.0))
        );
        assert_eq!(field_value(&doc, "e", &FieldOrder::Plain), None);
        assert_eq!(field_value(&doc, "z", &FieldOrder::Plain), None);
        assert_eq!(field_value(&doc, "missing", &FieldOrder::Plain), None);
    }
}
