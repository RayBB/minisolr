//! `/update`: Solr JSON update bodies and the command loop, as Python minisolr's
//! `parse_update_body` and `update`.
//!
//! A body is an array of documents, a single document, or a command object whose keys
//! may repeat (`{"add": {...}, "add": {...}, "delete": {...}, "commit": {}}`, which is
//! what Open Library sends). `serde_json::Value` keeps only the last of repeated keys,
//! so [`split_object`] splits the top-level object into ordered `(key, raw value)`
//! pairs and each value is parsed on its own.

use std::time::Instant;

use serde_json::{Value, json};

use crate::Error;
use crate::query::Params;
use crate::store::{Store, UpdateError};

/// The commands an update body can hold; a top-level object with none of them is a
/// single document.
const COMMANDS: [&str; 5] = ["add", "delete", "commit", "optimize", "rollback"];

/// Splits a JSON object into its top-level `(key, raw value)` pairs, in order, keeping
/// repeated keys. Values are returned unparsed (trimmed); strings in them may contain
/// braces, brackets and escaped quotes.
///
/// # Errors
/// [`Error::Other`] when `text` is not a well-formed object at the top level (values
/// are checked later, when they are parsed).
pub fn split_object(text: &str) -> Result<Vec<(String, &str)>, Error> {
    let bytes = text.as_bytes();
    let bad =
        |at: usize, what: &str| Error::Other(format!("invalid update body at byte {at}: {what}"));
    let mut i = skip_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return Err(bad(i, "expected '{'"));
    }
    i = skip_ws(bytes, i + 1);
    let mut pairs = Vec::new();
    if bytes.get(i) == Some(&b'}') {
        return trailing(bytes, i + 1)
            .map(|()| pairs)
            .map_err(|at| bad(at, "text after the object"));
    }
    loop {
        if bytes.get(i) != Some(&b'"') {
            return Err(bad(i, "expected a key"));
        }
        let key_end = string_end(bytes, i).ok_or_else(|| bad(i, "unterminated key"))?;
        let key: String = serde_json::from_str(text.get(i..key_end).unwrap_or_default())?;
        i = skip_ws(bytes, key_end);
        if bytes.get(i) != Some(&b':') {
            return Err(bad(i, "expected ':'"));
        }
        let value_start = skip_ws(bytes, i + 1);
        let value_end =
            value_end(bytes, value_start).ok_or_else(|| bad(value_start, "unterminated value"))?;
        let raw = text
            .get(value_start..value_end)
            .unwrap_or_default()
            .trim_end();
        if raw.is_empty() {
            return Err(bad(value_start, "missing value"));
        }
        pairs.push((key, raw));
        i = skip_ws(bytes, value_end);
        match bytes.get(i) {
            Some(b',') => i = skip_ws(bytes, i + 1),
            Some(b'}') => {
                return trailing(bytes, i + 1)
                    .map(|()| pairs)
                    .map_err(|at| bad(at, "text after the object"));
            }
            _ => return Err(bad(i, "expected ',' or '}'")),
        }
    }
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn trailing(bytes: &[u8], i: usize) -> Result<(), usize> {
    let end = skip_ws(bytes, i);
    if end == bytes.len() { Ok(()) } else { Err(end) }
}

/// The index just past the closing quote of the string starting at `start`.
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    loop {
        match bytes.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// The index just past the value starting at `start`: a string, a balanced object or
/// array (skipping strings inside), or a scalar running to the next `,` or `}`.
fn value_end(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start)? {
        b'"' => string_end(bytes, start),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = start;
            loop {
                match bytes.get(i)? {
                    b'"' => {
                        i = string_end(bytes, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => {
            let mut i = start;
            while bytes.get(i).is_some_and(|b| !matches!(b, b',' | b'}')) {
                i += 1;
            }
            Some(i)
        }
    }
}

/// An update body as ordered `(command, payload)` pairs: an array becomes one `add` per
/// document, a single document one `add`, and a command object its pairs in order.
/// A scalar body has no commands.
///
/// # Errors
/// Fails on malformed JSON.
pub fn parse_body(text: &str) -> Result<Vec<(String, Value)>, Error> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') {
        return Ok(match serde_json::from_str::<Value>(text)? {
            Value::Array(docs) => docs
                .into_iter()
                .map(|d| ("add".to_owned(), json!({ "doc": d })))
                .collect(),
            _ => Vec::new(),
        });
    }
    let pairs = split_object(text)?;
    if !pairs.is_empty() && !pairs.iter().any(|(k, _)| COMMANDS.contains(&k.as_str())) {
        // Python minisolr treats every top-level object as commands and so ignores a
        // single bare document; Solr's JSON loader adds it.
        return Ok(vec![(
            "add".to_owned(),
            json!({ "doc": serde_json::from_str::<Value>(text)? }),
        )]);
    }
    pairs
        .into_iter()
        .map(|(k, raw)| Ok((k, serde_json::from_str(raw)?)))
        .collect()
}

/// Returns the uniqueKeys of the rows matching a delete query in the store as it is.
pub type DeleteQuery<'a> = dyn FnMut(&Store, &str) -> Result<Vec<String>, Error> + 'a;

/// The response to an update request.
#[derive(Debug)]
pub struct Outcome {
    /// HTTP status.
    pub status: u16,
    /// The Solr-shaped JSON body.
    pub body: Value,
    /// Whether the store must be saved (something changed, or a commit was asked for).
    pub save: bool,
}

/// Applies an update body to the store, as Python's `update`.
///
/// Commands run in order. A document error ([`UpdateError::Doc`]) ends the request
/// with status 400, or with `update.chain` naming a tolerant chain is recorded in
/// `responseHeader.errors` and the next command runs. Any other failure ends the
/// request with status 400. Changes made before a failure stay in memory, as in Python.
/// `delete_query` returns the uniqueKeys of the rows matching a delete query in the
/// store as it is when called.
pub fn apply(
    store: &mut Store,
    body: &str,
    params: &Params,
    delete_query: &mut DeleteQuery<'_>,
) -> Outcome {
    let started = Instant::now();
    let tolerant = params
        .get("update.chain")
        .is_some_and(|c| c.contains("tolerant"));
    let flag = |name| matches!(params.get(name), Some("true" | ""));
    let mut commit = flag("commit") || flag("softCommit");
    let mut changed = false;
    let mut errors = Vec::new();
    let commands = match parse_body(body) {
        Ok(c) => c,
        Err(e) => return fatal(&format!("JSONDecodeError: {e}")),
    };
    for (cmd, payload) in commands {
        let result = match cmd.as_str() {
            "add" => add(store, &payload).map(|()| changed = true),
            "delete" => delete(store, &payload, delete_query).map(|()| changed = true),
            "commit" | "optimize" => {
                commit = true;
                Ok(())
            }
            _ => Ok(()),
        };
        match result {
            Ok(()) => {}
            Err(UpdateError::Doc(msg)) if tolerant => {
                errors.push(json!({"type": "ADD", "message": msg}))
            }
            Err(UpdateError::Doc(msg)) => {
                return Outcome {
                    status: 400,
                    body: json!({"responseHeader": {"status": 400, "QTime": 0}, "error": {"msg": msg, "code": 400}}),
                    save: false,
                };
            }
            Err(UpdateError::Fatal(msg)) => return fatal(&msg),
        }
    }
    let mut header = json!({"status": 0, "QTime": started.elapsed().as_millis()});
    if !errors.is_empty()
        && let Some(map) = header.as_object_mut()
    {
        map.insert("errors".into(), Value::Array(errors));
    }
    Outcome {
        status: 200,
        body: json!({ "responseHeader": header }),
        save: changed || commit,
    }
}

/// The shape of an uncaught exception in Python minisolr's update route.
fn fatal(msg: &str) -> Outcome {
    Outcome {
        status: 400,
        body: json!({"responseHeader": {"status": 400}, "error": {"msg": msg, "code": 400}}),
        save: false,
    }
}

/// `add`: `{"doc": ...}` or a bare document; either may be a list of documents.
fn add(store: &mut Store, payload: &Value) -> Result<(), UpdateError> {
    let docs = payload
        .get("doc")
        .filter(|_| payload.is_object())
        .unwrap_or(payload);
    match docs {
        Value::Array(items) => items.iter().try_for_each(|d| store.add(d)),
        doc => store.add(doc),
    }
}

/// `delete`: ids (a string, `{"id": ...}` or a list of them) and `{"query": ...}`.
fn delete(
    store: &mut Store,
    payload: &Value,
    delete_query: &mut DeleteQuery<'_>,
) -> Result<(), UpdateError> {
    let items = match payload {
        Value::Array(items) => items.as_slice(),
        one => std::slice::from_ref(one),
    };
    for item in items {
        if let Some(q) = item.get("query").filter(|_| item.is_object()) {
            let q = q.as_str().ok_or_else(|| {
                UpdateError::Fatal(format!("TypeError: delete query is not a string: {q}"))
            })?;
            let keys = delete_query(store, q)
                .map_err(|e| UpdateError::Fatal(format!("UnsupportedError: {e}")))?;
            for key in keys {
                store.delete_id(&key);
            }
        } else if item.is_object() {
            let id = item
                .get("id")
                .ok_or_else(|| UpdateError::Fatal("KeyError: 'id'".into()))?;
            store.delete_id(&id_text(id));
        } else {
            store.delete_id(&id_text(item));
        }
    }
    Ok(())
}

fn id_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn splits_repeated_keys_in_order() {
        let body = r#" {"add": {"doc": {"key": "a"}}, "add" : {"doc":{"key":"b"}},
            "delete": ["x", "y"], "commit": {} } "#;
        let pairs = split_object(body).expect("splits");
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["add", "add", "delete", "commit"]);
        assert_eq!(pairs.get(1).map(|p| p.1), Some(r#"{"doc":{"key":"b"}}"#));
        assert_eq!(pairs.get(2).map(|p| p.1), Some(r#"["x", "y"]"#));
        assert_eq!(pairs.get(3).map(|p| p.1), Some("{}"));
    }

    #[test]
    fn strings_with_braces_and_escaped_quotes() {
        let body = r#"{"add": {"doc": {"key": "/w/1", "title": "a } ] { [ \"quoted\" \\"}}, "k\"e}y": "v,}", "n": -1.5e3, "t": true, "z": null}"#;
        let pairs = split_object(body).expect("splits");
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["add", "k\"e}y", "n", "t", "z"]);
        let doc: Value = serde_json::from_str(pairs.first().map(|p| p.1).unwrap_or_default())
            .expect("value parses");
        assert_eq!(doc["doc"]["title"], "a } ] { [ \"quoted\" \\");
        assert_eq!(pairs.get(1).map(|p| p.1), Some(r#""v,}""#));
        assert_eq!(pairs.get(2).map(|p| p.1), Some("-1.5e3"));
        assert_eq!(split_object("{}").expect("empty").len(), 0);
        for bad in [
            "",
            "[]",
            "{",
            r#"{"a"}"#,
            r#"{"a": 1,}"#,
            r#"{"a": 1} x"#,
            r#"{"a": "x}"#,
            r"{a: 1}",
        ] {
            assert!(split_object(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn body_shapes() {
        let arr = parse_body(r#"[{"key": "a"}, {"key": "b"}]"#).expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(
            arr.first().map(|c| c.1["doc"]["key"].clone()),
            Some(json!("a"))
        );
        let single = parse_body(r#"{"key": "a", "title": "x"}"#).expect("doc");
        assert_eq!(
            single,
            vec![("add".to_owned(), json!({"doc": {"key": "a", "title": "x"}}))]
        );
        let cmds = parse_body(r#"{"delete": {"query": "key:\"/works/NOPE\""}, "commit": {}}"#)
            .expect("cmds");
        assert_eq!(
            cmds.first().map(|c| c.1["query"].clone()),
            Some(json!("key:\"/works/NOPE\""))
        );
        assert!(parse_body("\"x\"").expect("scalar").is_empty());
        assert!(parse_body("{\"add\": }").is_err());
        assert!(parse_body("").is_err());
    }

    fn store() -> Store {
        let conf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/conf");
        Store::new(
            Arc::new(crate::schema::Schema::load(&conf).expect("schema")),
            None,
        )
    }

    fn params(pairs: &[(&str, &str)]) -> Params {
        Params::new(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn by_prefix(store: &Store, q: &str) -> Result<Vec<String>, Error> {
        let prefix = q
            .strip_prefix("key:")
            .ok_or_else(|| Error::Unsupported(q.into()))?;
        Ok(store
            .rows()
            .iter()
            .filter_map(|d| d.get("key").and_then(Value::as_str))
            .filter(|k| k.starts_with(prefix))
            .map(str::to_owned)
            .collect())
    }

    #[test]
    fn commands_run_in_order_with_deletes() {
        let mut s = store();
        let body = r#"{"add": {"doc": {"key": "/works/1", "editions": [{"key": "/books/1"}]}},
                      "add": [{"key": "/works/2"}, {"key": "/works/3"}],
                      "add": {"key": "/works/4"},
                      "delete": "/works/2", "delete": {"id": "/works/3"}, "delete": {"query": "key:/works/1"}}"#;
        let out = apply(&mut s, body, &params(&[]), &mut by_prefix);
        assert_eq!(out.status, 200, "{}", out.body);
        assert!(out.save);
        let keys: Vec<String> = s
            .rows()
            .iter()
            .filter_map(|d| d["key"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(keys, ["/works/4"], "delete by query takes the children too");
        let commit = apply(&mut s, r#"{"commit": {}}"#, &params(&[]), &mut by_prefix);
        assert!(commit.save);
        let nothing = apply(&mut s, "[]", &params(&[]), &mut by_prefix);
        assert!(!nothing.save);
        assert!(apply(&mut s, "[]", &params(&[("commit", "true")]), &mut by_prefix).save);
        assert!(
            apply(
                &mut s,
                "[]",
                &params(&[
                    ("softCommit", "true"),
                    ("update.partial.requireInPlace", "true")
                ]),
                &mut by_prefix
            )
            .save
        );
    }

    #[test]
    fn tolerant_chain_collects_errors() {
        let body = r#"{"add": {"doc": {"title": "no key"}}, "add": {"doc": {"key": "/works/OK"}},
                      "add": {"doc": {"key": "/works/NOPE", "title": {"set": "x"}}}}"#;
        let mut s = store();
        let out = apply(
            &mut s,
            body,
            &params(&[("update.chain", "tolerant-chain")]),
            &mut by_prefix,
        );
        assert_eq!(out.status, 200);
        assert_eq!(out.body["responseHeader"]["status"], 0);
        assert_eq!(
            out.body["responseHeader"]["errors"],
            json!([
                {"type": "ADD", "message": "Document is missing mandatory uniqueKey field: key"},
                {"type": "ADD", "message": "Document not found for update.  id=/works/NOPE"}
            ])
        );
        assert!(s.get("/works/OK").is_some());

        let mut s = store();
        let out = apply(&mut s, body, &params(&[]), &mut by_prefix);
        assert_eq!(out.status, 400);
        assert_eq!(
            out.body,
            json!({"responseHeader": {"status": 400, "QTime": 0}, "error": {"msg": "Document is missing mandatory uniqueKey field: key", "code": 400}})
        );
        assert!(!out.save);
        assert!(
            s.get("/works/OK").is_none(),
            "commands after the failure do not run"
        );

        let out = apply(
            &mut s,
            r#"{"delete": {"query": "title:x"}}"#,
            &params(&[("update.chain", "tolerant")]),
            &mut by_prefix,
        );
        assert_eq!(
            out.status, 400,
            "a bad delete query is fatal even when tolerant"
        );
        assert_eq!(out.body["responseHeader"], json!({"status": 400}));
        let out = apply(
            &mut s,
            r#"{"delete": {"nope": 1}}"#,
            &params(&[]),
            &mut by_prefix,
        );
        assert_eq!(out.body["error"]["msg"], "KeyError: 'id'");
        let out = apply(&mut s, "{not json", &params(&[]), &mut by_prefix);
        assert_eq!(out.status, 400);
    }
}
