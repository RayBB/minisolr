//! Local params: `{!type key=value ...}` syntax, `$param` dereference, and compiling the
//! parser types `edismax`/`dismax`, `lucene`, `terms`, `field`, `prefix` and `parent`.

use std::borrow::Cow;

use tantivy::Term;
use tantivy::query::{EmptyQuery, Query, TermSetQuery};

use super::Params;
use super::ast::{LocalParams, ParseError};
use super::compile::{Compiler, Ctx, LeafOpts, QueryResult};
use crate::Error;
use crate::schema::Kind;

/// Parses local params; `pos` is the byte offset just after `{!`. Returns the params and
/// the offset after the closing `}`.
///
/// Values may be bare (up to a space or `}`), `"double quoted"` with `\"` escapes, or
/// `'single quoted'`. A first word containing `=` means there is no type, which is
/// `lucene` (`{!tag=x}`).
///
/// # Errors
/// [`ParseError`] when the `}` or a quote is missing, or a key is not `[\w.]+=`.
pub fn parse(s: &str, pos: usize) -> Result<(LocalParams<'_>, usize), ParseError> {
    let rest = s.get(pos..).unwrap_or_default();
    let type_len = rest
        .find(|c: char| c.is_whitespace() || c == '}' || c == '=')
        .unwrap_or(rest.len());
    let (kind, mut j) = if rest.get(type_len..).is_some_and(|r| r.starts_with('=')) {
        ("lucene", pos)
    } else {
        (rest.get(..type_len).unwrap_or_default(), pos + type_len)
    };
    let mut params = Vec::new();
    loop {
        let tail = s.get(j..).unwrap_or_default();
        j += tail.len() - tail.trim_start().len();
        let tail = s.get(j..).unwrap_or_default();
        if tail.is_empty() {
            return Err(ParseError::UnterminatedLocalParams);
        }
        if tail.starts_with('}') {
            return Ok((LocalParams { kind, params }, j + 1));
        }
        let key_len = tail
            .find(|c: char| !(c == '_' || c == '.' || c.is_alphanumeric()))
            .unwrap_or(tail.len());
        if key_len == 0 || !tail.get(key_len..).is_some_and(|r| r.starts_with('=')) {
            return Err(ParseError::BadLocalParam(j));
        }
        let key = tail.get(..key_len).unwrap_or_default();
        j += key_len + 1;
        let value_text = s.get(j..).unwrap_or_default();
        let (value, used) = match value_text.chars().next() {
            Some('"') => {
                let body = value_text.get(1..).unwrap_or_default();
                let mut chars = body.char_indices();
                let close = loop {
                    match chars.next() {
                        Some((_, '\\')) => {
                            if chars.next().is_none() {
                                return Err(ParseError::UnterminatedLocalParams);
                            }
                        }
                        Some((k, '"')) => break k,
                        Some(_) => {}
                        None => return Err(ParseError::UnterminatedLocalParams),
                    }
                };
                let raw = body.get(..close).unwrap_or_default();
                let value = if raw.contains("\\\"") {
                    Cow::Owned(raw.replace("\\\"", "\""))
                } else {
                    Cow::Borrowed(raw)
                };
                (value, close + 2)
            }
            Some('\'') => {
                let body = value_text.get(1..).unwrap_or_default();
                let close = body.find('\'').ok_or(ParseError::UnterminatedLocalParams)?;
                (
                    Cow::Borrowed(body.get(..close).unwrap_or_default()),
                    close + 2,
                )
            }
            Some(_) => {
                let end = value_text.find([' ', '}']).unwrap_or(value_text.len());
                (
                    Cow::Borrowed(value_text.get(..end).unwrap_or_default()),
                    end,
                )
            }
            None => return Err(ParseError::UnterminatedLocalParams),
        };
        params.push((key, value));
        j += used;
    }
}

/// Resolves a `$name` reference to the request parameter (empty when absent); other
/// values are returned as they are. One level only, as in Solr.
#[must_use]
pub fn deref<'a>(params: &'a Params, v: &'a str) -> &'a str {
    match v.strip_prefix('$') {
        Some(name) => params.get(name).unwrap_or_default(),
        None => v,
    }
}

impl Compiler<'_> {
    /// Compiles `{!type ...}` in context `ctx`.
    pub(super) fn local_params(&mut self, local: &LocalParams<'_>, ctx: Ctx<'_>) -> QueryResult {
        let params = self.params;
        let v = deref(params, local.get("v").unwrap_or_default());
        match local.kind {
            "edismax" | "dismax" => self.edismax(v, Some(local)),
            "lucene" | "" => self.compile_str(v, ctx.child()),
            "terms" => self.terms(local, v),
            "field" => {
                let field = required(local, "f")?;
                self.leaf(field, v, ctx, LeafOpts::default())
            }
            "prefix" => {
                let field = required(local, "f")?;
                self.leaf(field, &format!("{v}*"), ctx, LeafOpts::default())
            }
            "parent" => {
                let filters: Vec<&str> = match local.get("filters") {
                    Some(f) if f.starts_with('$') => {
                        params.all(f.get(1..).unwrap_or_default()).collect()
                    }
                    Some("") | None => Vec::new(),
                    Some(f) => vec![f],
                };
                self.parent_join(v, &filters)
            }
            other => Err(Error::Unsupported(format!(
                "local params type {{!{other}}}"
            ))),
        }
    }

    /// `{!terms f=field v=a,b,c}` (optional `separator=`): any of the values, constant score.
    fn terms(&self, local: &LocalParams<'_>, v: &str) -> QueryResult {
        let name = required(local, "f")?;
        let separator = local.get("separator").unwrap_or(",");
        let values: Vec<&str> = if separator.is_empty() {
            vec![v]
        } else {
            v.split(separator).filter(|x| !x.is_empty()).collect()
        };
        let Some(def) = self.store.schema.get(name) else {
            return Ok(Some(Box::new(EmptyQuery)));
        };
        let Some(indexed) = self.store.fields.get(name) else {
            return Ok(Some(Box::new(EmptyQuery)));
        };
        if values.is_empty() {
            return Ok(Some(Box::new(EmptyQuery)));
        }
        let field = indexed.handle;
        let terms: Vec<Term> = match def.kind {
            Kind::Int | Kind::Float => values
                .iter()
                .map(|x| {
                    x.trim()
                        .parse::<f64>()
                        .map(|n| Term::from_field_f64(field, n))
                        .map_err(|_| {
                            Error::Unsupported(format!("{{!terms}} value {x:?} on {name}"))
                        })
                })
                .collect::<Result<_, _>>()?,
            Kind::Text => values
                .iter()
                .flat_map(|x| self.store.analyzer.query(def.chain, x))
                .map(|t| Term::from_field_text(field, &t))
                .collect(),
            _ => values
                .iter()
                .map(|x| Term::from_field_text(field, x))
                .collect(),
        };
        if terms.is_empty() {
            return Ok(Some(Box::new(EmptyQuery)));
        }
        Ok(Some(Box::new(TermSetQuery::new(terms)) as Box<dyn Query>))
    }
}

fn required<'l>(local: &'l LocalParams<'_>, key: &str) -> Result<&'l str, Error> {
    local
        .get(key)
        .ok_or_else(|| Error::Unsupported(format!("{{!{}}} without {key}=", local.kind)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_values_and_quotes() {
        let s = r#"{!edismax q.op="AND" qf="a^2 \"b\"" v=$uq}rest"#;
        let (local, end) = parse(s, 2).expect("parses");
        assert_eq!(local.kind, "edismax");
        assert_eq!(local.get("q.op"), Some("AND"));
        assert_eq!(local.get("qf"), Some("a^2 \"b\""));
        assert_eq!(local.get("v"), Some("$uq"));
        assert_eq!(s.get(end..), Some("rest"));

        let (tagged, _) = parse("{!tag=lang}x", 2).expect("parses");
        assert_eq!(tagged.kind, "lucene");
        assert_eq!(tagged.get("tag"), Some("lang"));

        let (single, _) = parse("{!terms f=key v='/a,/b c'}", 2).expect("parses");
        assert_eq!(single.get("v"), Some("/a,/b c"));

        let (bare, end) = parse("{!edismax}", 2).expect("parses");
        assert_eq!((bare.kind, bare.params.len(), end), ("edismax", 0, 10));
    }

    #[test]
    fn malformed_local_params() {
        assert_eq!(
            parse("{!x a=1", 2).map(|_| ()),
            Err(ParseError::UnterminatedLocalParams)
        );
        assert_eq!(
            parse("{!x a=\"open}", 2).map(|_| ()),
            Err(ParseError::UnterminatedLocalParams)
        );
        assert_eq!(
            parse("{!x a='open}", 2).map(|_| ()),
            Err(ParseError::UnterminatedLocalParams)
        );
        assert_eq!(
            parse("{!x =1}", 2).map(|_| ()),
            Err(ParseError::BadLocalParam(4))
        );
        assert_eq!(
            parse("{!x a=", 2).map(|_| ()),
            Err(ParseError::UnterminatedLocalParams)
        );
    }

    #[test]
    fn dereference() {
        let p = Params::new(vec![("uq".into(), "twain".into())]);
        assert_eq!(deref(&p, "$uq"), "twain");
        assert_eq!(deref(&p, "$missing"), "");
        assert_eq!(deref(&p, "plain"), "plain");
    }
}
