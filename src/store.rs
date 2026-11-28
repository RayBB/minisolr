//! The document store: the source of truth, kept in memory and persisted as JSON lines.
//!
//! Mirrors Python minisolr's `Store`: documents keyed by the uniqueKey in insertion order
//! (a re-added document moves to the end, an updated one keeps its place), nested
//! children as separate rows carrying `_root_` and `_nest_path_`, atomic updates, and a
//! version that moves on every change so the derived index can be rebuilt lazily. The
//! file is `<data dir>/docs.jsonl`, one JSON object per line, written to `docs.tmp` and
//! renamed, so Python minisolr and this server can read each other's data directories.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Number, Value};

use crate::Error;
use crate::schema::Schema;

/// The atomic update operations Python minisolr recognises (`removeregex` is accepted
/// and, as in Python, does nothing).
pub const ATOMIC_OPS: [&str; 6] = ["set", "add", "add-distinct", "remove", "removeregex", "inc"];

/// Why an update command failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// A document-level failure (Python's `UpdateError`): recorded and skipped by the
    /// tolerant update chain, otherwise it ends the request with status 400.
    Doc(String),
    /// Any other failure (a malformed command, an unsupported delete query, a type
    /// error in `inc`): it always ends the request, as an uncaught exception in Python.
    Fatal(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Doc(msg) | Self::Fatal(msg) => f.write_str(msg),
        }
    }
}

/// Documents in insertion order, keyed by the uniqueKey.
pub struct Store {
    schema: Arc<Schema>,
    /// Rows in order; a deleted row leaves `None` until the next compaction.
    slots: Vec<Option<(String, Arc<Value>)>>,
    by_key: HashMap<String, usize>,
    path: Option<PathBuf>,
    /// Moves on every change.
    pub version: u64,
}

impl Store {
    /// An empty store; with a data directory, documents persist in `<dir>/docs.jsonl`.
    #[must_use]
    pub fn new(schema: Arc<Schema>, data_dir: Option<&Path>) -> Self {
        Self {
            schema,
            slots: Vec::new(),
            by_key: HashMap::new(),
            path: data_dir.map(|d| d.join("docs.jsonl")),
            version: 0,
        }
    }

    /// Opens a data directory: loads `<dir>/docs.jsonl` when it exists.
    ///
    /// # Errors
    /// Fails if the file cannot be read, a line is not a JSON object, or a row has no
    /// uniqueKey.
    pub fn open(schema: Arc<Schema>, data_dir: &Path) -> Result<Self, Error> {
        let mut store = Self::new(schema, Some(data_dir));
        if let Some(path) = store.path.clone()
            && path.exists()
        {
            store.load_file(&path)?;
        }
        Ok(store)
    }

    /// Adds the rows of a JSON-lines file as they are (no `_root_` is added), later rows
    /// replacing earlier ones with the same key in place, as Python's `_load`.
    ///
    /// # Errors
    /// As [`Store::open`].
    pub fn load_file(&mut self, path: &Path) -> Result<(), Error> {
        for (n, line) in BufReader::new(File::open(path)?).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let row: Value = serde_json::from_str(&line)?;
            let key = row
                .get(&self.schema.unique_key)
                .map(key_text)
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    Error::Other(format!(
                        "{}:{}: row without {}",
                        path.display(),
                        n + 1,
                        self.schema.unique_key
                    ))
                })?;
            self.put(key, row);
        }
        self.version += 1;
        Ok(())
    }

    /// Number of rows (children included).
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether the store has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Whether updates are written to disk.
    #[must_use]
    pub fn persistent(&self) -> bool {
        self.path.is_some()
    }

    /// The row stored under `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        let slot = *self.by_key.get(key)?;
        self.slots.get(slot)?.as_ref().map(|(_, d)| &**d)
    }

    /// The rows in order, shared with the store (cheap to take).
    #[must_use]
    pub fn rows(&self) -> Vec<Arc<Value>> {
        self.slots
            .iter()
            .flatten()
            .map(|(_, d)| Arc::clone(d))
            .collect()
    }

    /// Inserts or replaces a row; a new key goes to the end, an existing one keeps its
    /// place (Python dict assignment).
    fn put(&mut self, key: String, row: Value) {
        if let Some(slot) = self.by_key.get(&key).and_then(|&i| self.slots.get_mut(i)) {
            *slot = Some((key, Arc::new(row)));
            return;
        }
        self.by_key.insert(key.clone(), self.slots.len());
        self.slots.push(Some((key, Arc::new(row))));
    }

    fn remove(&mut self, key: &str) {
        if let Some(slot) = self.by_key.remove(key).and_then(|i| self.slots.get_mut(i)) {
            *slot = None;
        }
        if self.slots.len() > 64 && self.by_key.len() < self.slots.len() / 2 {
            self.compact();
        }
    }

    fn compact(&mut self) {
        self.slots.retain(Option::is_some);
        self.by_key = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|(k, _)| (k.clone(), i)))
            .collect();
    }

    /// The key itself and every row whose `_root_` is `key` (Python's `_family`).
    fn family(&self, key: &str) -> Vec<String> {
        self.slots
            .iter()
            .flatten()
            .filter(|(k, d)| k == key || d.get("_root_").and_then(Value::as_str) == Some(key))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Deletes a document and its children; a missing key is not an error.
    pub fn delete_id(&mut self, key: &str) {
        for k in self.family(key) {
            self.remove(&k);
        }
        self.version += 1;
    }

    /// Adds (or replaces) a document, as Python's `Store.add`: a document with an atomic
    /// operation on any field updates the stored one; otherwise the old family is
    /// replaced, `_root_` is set to the key and the `editions` / `_childDocuments_`
    /// children become rows with `_root_` and `_nest_path_ = /editions#i`.
    ///
    /// # Errors
    /// [`UpdateError::Doc`] for a missing uniqueKey (also on a child) or an atomic update
    /// of a missing document; [`UpdateError::Fatal`] when `doc` is not an object.
    pub fn add(&mut self, doc: &Value) -> Result<(), UpdateError> {
        let uk = self.schema.unique_key.clone();
        let Value::Object(map) = doc else {
            return Err(UpdateError::Fatal(format!(
                "AttributeError: document is not an object: {doc}"
            )));
        };
        let Some(key) = map.get(&uk).filter(|v| truthy(v)).map(key_text) else {
            return Err(UpdateError::Doc(format!(
                "Document is missing mandatory uniqueKey field: {uk}"
            )));
        };
        if map.iter().any(|(k, v)| *k != uk && is_atomic(v)) {
            return self.atomic(&key, map);
        }
        let mut map = map.clone();
        let mut children = Vec::new();
        for name in ["editions", "_childDocuments_"] {
            match map.remove(name) {
                Some(Value::Array(items)) => children.extend(items),
                Some(Value::Null) | None => {}
                Some(other) => children.push(other),
            }
        }
        for k in self.family(&key) {
            self.remove(&k);
        }
        map.insert("_root_".into(), Value::String(key.clone()));
        self.put(key.clone(), Value::Object(map));
        self.version += 1;
        for (i, raw) in children.into_iter().enumerate() {
            let Value::Object(mut child) = raw else {
                return Err(UpdateError::Fatal(format!(
                    "TypeError: child document of {key} is not an object"
                )));
            };
            let Some(ck) = child.get(&uk).filter(|v| truthy(v)).map(key_text) else {
                return Err(UpdateError::Doc(format!(
                    "Child document of {key} is missing {uk}"
                )));
            };
            child.insert("_root_".into(), Value::String(key.clone()));
            child.insert(
                "_nest_path_".into(),
                Value::String(format!("/editions#{i}")),
            );
            self.put(ck, Value::Object(child));
        }
        Ok(())
    }

    /// Python's `Store._atomic`: plain fields are replaced; `set` (`null` removes the
    /// field), `inc`, `add`, `add-distinct` and `remove` change the stored document in
    /// place.
    fn atomic(&mut self, key: &str, update: &Map<String, Value>) -> Result<(), UpdateError> {
        let uk = self.schema.unique_key.as_str();
        let Some((_, current)) = self
            .by_key
            .get(key)
            .and_then(|&i| self.slots.get_mut(i))
            .and_then(Option::as_mut)
        else {
            return Err(UpdateError::Doc(format!(
                "Document not found for update.  id={key}"
            )));
        };
        let cur = Arc::make_mut(current);
        let Value::Object(cur) = cur else {
            return Err(UpdateError::Fatal(format!(
                "stored row {key} is not an object"
            )));
        };
        for (field, op) in update {
            if field == uk {
                continue;
            }
            let Value::Object(ops) = op else {
                cur.insert(field.clone(), op.clone());
                continue;
            };
            if !is_atomic(op) {
                cur.insert(field.clone(), op.clone());
                continue;
            }
            for (name, val) in ops {
                if let Err(e) = apply_op(cur, field, name, val) {
                    // Python stops at the failing operation; earlier ones stay applied.
                    self.version += 1;
                    return Err(e);
                }
            }
        }
        self.version += 1;
        Ok(())
    }

    /// Writes `<data dir>/docs.jsonl` (to `docs.tmp`, then renamed); a store without a
    /// data directory does nothing.
    ///
    /// # Errors
    /// Fails if the directory or file cannot be written.
    pub fn save(&self) -> Result<(), Error> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut out = BufWriter::new(File::create(&tmp)?);
            for (_, row) in self.slots.iter().flatten() {
                serde_json::to_writer(&mut out, &**row)?;
                out.write_all(b"\n")?;
            }
            out.into_inner()
                .map_err(|e| Error::Io(e.into_error()))?
                .sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn apply_op(
    cur: &mut Map<String, Value>,
    field: &str,
    name: &str,
    val: &Value,
) -> Result<(), UpdateError> {
    match name {
        "set" if val.is_null() => {
            cur.remove(field);
        }
        "set" => {
            cur.insert(field.to_owned(), val.clone());
        }
        "inc" => {
            let base = cur.get(field).filter(|v| truthy(v)).cloned();
            let sum = add_numbers(base.as_ref(), val).ok_or_else(|| {
                UpdateError::Fatal(format!(
                    "TypeError: cannot inc {field} ({}) by {val}",
                    base.unwrap_or(Value::from(0))
                ))
            })?;
            cur.insert(field.to_owned(), sum);
        }
        "add" | "add-distinct" => {
            let mut existing = cur.get(field).map_or_else(Vec::new, as_list);
            for v in as_list(val) {
                if name == "add" || !existing.contains(&v) {
                    existing.push(v);
                }
            }
            cur.insert(field.to_owned(), Value::Array(existing));
        }
        "remove" => {
            let drop = as_list(val);
            let kept = cur
                .get(field)
                .map_or_else(Vec::new, as_list)
                .into_iter()
                .filter(|v| !drop.contains(v))
                .collect();
            cur.insert(field.to_owned(), Value::Array(kept));
        }
        _ => {}
    }
    Ok(())
}

/// `base + val` with Python's number rules: integers stay integers, anything with a
/// float becomes a float, a missing or falsy base counts as 0. `None` for non-numbers.
fn add_numbers(base: Option<&Value>, val: &Value) -> Option<Value> {
    let zero = Number::from(0);
    let a = match base {
        None => &zero,
        Some(Value::Number(n)) => n,
        Some(_) => return None,
    };
    let Value::Number(b) = val else {
        return None;
    };
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64())
        && let Some(sum) = x.checked_add(y)
    {
        return Some(Value::from(sum));
    }
    if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64())
        && let Some(sum) = x.checked_add(y)
    {
        return Some(Value::from(sum));
    }
    let sum = a.as_f64()? + b.as_f64()?;
    Number::from_f64(sum).map(Value::Number)
}

/// Whether a field value is an object naming at least one atomic operation.
fn is_atomic(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|m| m.keys().any(|k| ATOMIC_OPS.contains(&k.as_str())))
}

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A uniqueKey value as the map key: strings as they are, other scalars as JSON text.
fn key_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python's `as_list`: the items of an array, or the value itself.
fn as_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Arc<Schema> {
        let conf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/conf");
        Arc::new(Schema::load(&conf).expect("test schema"))
    }

    fn keys(store: &Store) -> Vec<String> {
        store
            .rows()
            .iter()
            .filter_map(|d| d.get("key").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    fn work() -> Value {
        json!({"key": "/works/OL1W", "type": "work", "title": "Alice", "edition_count": 2, "subject": ["a", "b"],
               "editions": [{"key": "/books/OL1M", "type": "edition"}, {"key": "/books/OL2M", "type": "edition"}]})
    }

    #[test]
    fn nested_children_become_rows_and_readd_replaces_family() {
        let mut s = Store::new(schema(), None);
        s.add(&json!({"key": "/authors/OL1A", "type": "author"}))
            .expect("add");
        s.add(&work()).expect("add");
        assert_eq!(
            keys(&s),
            ["/authors/OL1A", "/works/OL1W", "/books/OL1M", "/books/OL2M"]
        );
        let child = s.get("/books/OL2M").expect("child");
        assert_eq!(child["_root_"], "/works/OL1W");
        assert_eq!(child["_nest_path_"], "/editions#1");
        assert_eq!(s.get("/works/OL1W").expect("root")["_root_"], "/works/OL1W");
        assert!(
            s.get("/works/OL1W")
                .expect("root")
                .get("editions")
                .is_none()
        );

        let v = s.version;
        s.add(&json!({"key": "/works/OL1W", "title": "Alice 2", "_childDocuments_": [{"key": "/books/OL3M"}]}))
            .expect("re-add");
        assert!(s.version > v);
        assert_eq!(keys(&s), ["/authors/OL1A", "/works/OL1W", "/books/OL3M"]);
        assert_eq!(
            s.get("/books/OL3M").expect("child")["_nest_path_"],
            "/editions#0"
        );
    }

    #[test]
    fn missing_unique_key_is_a_document_error() {
        let mut s = Store::new(schema(), None);
        for bad in [
            json!({"title": "x"}),
            json!({"key": ""}),
            json!({"key": null}),
        ] {
            assert_eq!(
                s.add(&bad),
                Err(UpdateError::Doc(
                    "Document is missing mandatory uniqueKey field: key".into()
                ))
            );
        }
        let e = s.add(&json!({"key": "/works/OL1W", "editions": [{"title": "no key"}]}));
        assert_eq!(
            e,
            Err(UpdateError::Doc(
                "Child document of /works/OL1W is missing key".into()
            ))
        );
        assert!(matches!(s.add(&json!("x")), Err(UpdateError::Fatal(_))));
        assert!(
            s.get("/works/OL1W").is_some(),
            "the parent stays, as in Python"
        );
    }

    #[test]
    fn atomic_operations() {
        let mut s = Store::new(schema(), None);
        s.add(&work()).expect("add");
        let update = json!({"key": "/works/OL1W",
            "edition_count": {"inc": 3}, "title": {"set": "Alice II"}, "subject": {"add": ["b", "c"]},
            "subject_key": {"add-distinct": "x"}, "type": {"set": null}, "plain": 7, "rank_f": {"inc": 0.5}});
        s.add(&update).expect("atomic");
        let doc = s.get("/works/OL1W").expect("doc").clone();
        assert_eq!(doc["edition_count"], 5);
        assert_eq!(doc["title"], "Alice II");
        assert_eq!(doc["subject"], json!(["a", "b", "b", "c"]));
        assert_eq!(doc["subject_key"], json!(["x"]));
        assert!(doc.get("type").is_none(), "set null removes");
        assert_eq!(doc["plain"], 7);
        assert_eq!(doc["rank_f"], 0.5);
        assert_eq!(doc["_root_"], "/works/OL1W");

        s.add(&json!({"key": "/works/OL1W", "subject": {"add-distinct": ["a", "d"]}, "subject_key": {"remove": "x"},
                      "missing": {"remove": "y"}, "edition_count": {"inc": 1.5}, "title": {"removeregex": ".*"}}))
            .expect("atomic");
        let doc = s.get("/works/OL1W").expect("doc");
        assert_eq!(doc["subject"], json!(["a", "b", "b", "c", "d"]));
        assert_eq!(doc["subject_key"], json!([]));
        assert_eq!(
            doc["missing"],
            json!([]),
            "remove on a missing field leaves []"
        );
        assert_eq!(doc["edition_count"], 6.5);
        assert_eq!(
            doc["title"], "Alice II",
            "removeregex does nothing, as in Python"
        );
        assert_eq!(
            keys(&s),
            ["/works/OL1W", "/books/OL1M", "/books/OL2M"],
            "updates keep the place"
        );

        assert_eq!(
            s.add(&json!({"key": "/works/NOPE", "title": {"set": "x"}})),
            Err(UpdateError::Doc(
                "Document not found for update.  id=/works/NOPE".into()
            ))
        );
        assert!(matches!(
            s.add(&json!({"key": "/works/OL1W", "title": {"inc": 1}})),
            Err(UpdateError::Fatal(_))
        ));
        s.add(&json!({"key": "/books/OL1M", "title": {"set": "child"}}))
            .expect("atomic on a child");
        assert_eq!(s.get("/books/OL1M").expect("child")["title"], "child");
    }

    #[test]
    fn delete_removes_children_and_readd_goes_last() {
        let mut s = Store::new(schema(), None);
        s.add(&work()).expect("add");
        s.add(&json!({"key": "/authors/OL1A"})).expect("add");
        s.delete_id("/works/OL1W");
        assert_eq!(keys(&s), ["/authors/OL1A"]);
        s.delete_id("/works/NOPE");
        s.add(&work()).expect("add");
        s.delete_id("/books/OL1M");
        assert_eq!(keys(&s), ["/authors/OL1A", "/works/OL1W", "/books/OL2M"]);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn many_deletes_compact_and_keep_order() {
        let mut s = Store::new(schema(), None);
        for i in 0..200 {
            s.add(&json!({"key": format!("/k/{i}")})).expect("add");
        }
        for i in 0..190 {
            s.delete_id(&format!("/k/{i}"));
        }
        assert!(s.slots.len() < 200, "compacted");
        let want: Vec<String> = (190..200).map(|i| format!("/k/{i}")).collect();
        assert_eq!(keys(&s), want);
        assert!(s.get("/k/195").is_some());
    }

    #[test]
    fn save_and_open_round_trip() {
        let dir = std::env::temp_dir().join(format!("minisolr-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Store::open(schema(), &dir).expect("open empty");
        assert!(s.is_empty() && s.persistent());
        s.add(&work()).expect("add");
        s.add(&json!({"key": "/authors/OL1A", "name": "Zoë", "rank": 1.5}))
            .expect("add");
        s.save().expect("save");
        assert!(!dir.join("docs.tmp").exists());
        let back = Store::open(schema(), &dir).expect("reopen");
        assert_eq!(keys(&back), keys(&s));
        assert_eq!(back.rows(), s.rows());
        assert!(back.version > 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
