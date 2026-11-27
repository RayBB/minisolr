//! The RAM Tantivy index: a read-only snapshot of the store, built from the parsed schema.
//!
//! Text fields are analyzed here and indexed as space-joined tokens on Tantivy's
//! `whitespace` tokenizer; strings, enums, dates and booleans on `raw`; numbers as `f64`.
//! Like Python's `Store._rebuild`, every static field that is searchable is in the index,
//! plus each dynamic field name that occurs in a document. copyField values are applied
//! at index time (and again when a document is returned); the documents stay as stored.
//! Nested children are separate rows sharing `_root_`. A fast `ord` field maps a hit back
//! to its row. The index is a derived cache: [`crate::server::Core`] rebuilds it when the
//! store version moves on and a search arrives.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};
use tantivy::schema::{Field, IndexRecordOption, NumericOptions, TextFieldIndexing, TextOptions};
use tantivy::{IndexReader, ReloadPolicy, TantivyDocument};

use crate::Error;
use crate::analysis::{Analyzer, fold};
use crate::schema::{self, Kind, Schema};

/// A schema field that is in the Tantivy index.
#[derive(Debug)]
pub struct Indexed {
    /// The schema definition (concrete name for dynamic fields).
    pub def: schema::Field,
    /// The Tantivy field handle.
    pub handle: Field,
    /// The field type's index-time analyzer (text fields).
    pub index_analyzer: Arc<Analyzer>,
    /// The field type's query-time analyzer (text fields).
    pub query_analyzer: Arc<Analyzer>,
}

/// A snapshot of the documents and their index.
pub struct Index {
    /// The schema the index was built from.
    pub schema: Arc<Schema>,
    /// The spellcheck dictionary, sorted: every whitespace-separated word longer than
    /// three characters in the `title`, `author_name` and `name` values, folded (Python's
    /// `Store.term_dict`).
    pub term_dict: Vec<String>,
    /// Rows in `ord` order (the store's order), as stored.
    pub docs: Vec<Arc<Value>>,
    /// The store version this snapshot was built from.
    pub version: u64,
    pub(crate) reader: IndexReader,
    pub(crate) fields: HashMap<String, Indexed>,
    by_key: HashMap<String, usize>,
    /// Facet columns built so far, by field and keying.
    facet_columns: Mutex<HashMap<(String, Keying), Arc<FacetColumn>>>,
}

/// How facet values are told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keying {
    /// By Python's `str()` of the value, as `facet.field` counts (`true`/`false` for
    /// booleans).
    Text,
    /// By the JSON value itself, as JSON Facet API `terms` buckets count.
    Json,
}

/// The distinct values of one field in every row, so facets count hits without
/// reading documents.
#[derive(Debug, Default)]
pub struct FacetColumn {
    /// The keys of the distinct values, by value id.
    pub keys: Vec<String>,
    /// The first value seen for each key, by value id.
    pub values: Vec<Value>,
    /// For each row (by `ord`), the ids of its distinct values.
    pub rows: Vec<Vec<usize>>,
}

impl FacetColumn {
    fn build(schema: &Schema, docs: &[Arc<Value>], field: &str, keying: Keying) -> Self {
        let empty = Map::new();
        let mut ids: HashMap<String, usize> = HashMap::new();
        let mut col = Self::default();
        for doc in docs {
            let mut row = Vec::new();
            for v in schema
                .values_of(object(doc, &empty), field)
                .unwrap_or_default()
            {
                let key = match keying {
                    Keying::Text => facet_text(v),
                    Keying::Json => v.to_string(),
                };
                let id = *ids.entry(key).or_insert_with_key(|k| {
                    col.keys.push(k.clone());
                    col.values.push(v.clone());
                    col.keys.len() - 1
                });
                if !row.contains(&id) {
                    row.push(id);
                }
            }
            col.rows.push(row);
        }
        col
    }
}

/// Python's `str()` of a facet value, with booleans as Solr writes them.
#[must_use]
pub fn facet_text(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        other => crate::query::py_str(other),
    }
}

impl Index {
    /// Loads a JSON-lines file of documents and indexes them in RAM (for tests and tools).
    ///
    /// # Errors
    /// Fails if the file cannot be read or parsed, or if Tantivy fails.
    pub fn load(schema: Schema, path: &Path) -> Result<Self, Error> {
        let mut docs = Vec::new();
        for line in BufReader::new(std::fs::File::open(path)?).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                docs.push(Arc::new(serde_json::from_str::<Value>(&line)?));
            }
        }
        Self::from_docs(Arc::new(schema), docs, 0)
    }

    /// Indexes rows in RAM.
    ///
    /// # Errors
    /// Fails if Tantivy fails.
    pub fn from_docs(
        schema: Arc<Schema>,
        docs: Vec<Arc<Value>>,
        version: u64,
    ) -> Result<Self, Error> {
        let empty = Map::new();
        let mut defs: HashMap<String, schema::Field> = schema
            .fields
            .values()
            .filter(|f| f.in_index())
            .map(|f| (f.name.clone(), f.clone()))
            .collect();
        for doc in &docs {
            for name in schema.effective(object(doc, &empty)).keys() {
                if !defs.contains_key(name)
                    && !schema.fields.contains_key(name)
                    && let Some(f) = schema.get(name).filter(|f| f.in_index())
                {
                    defs.insert(name.clone(), f.into_owned());
                }
            }
        }
        let mut builder = tantivy::schema::Schema::builder();
        let ord = builder.add_u64_field("ord", NumericOptions::default().set_fast());
        let mut names: Vec<String> = defs.keys().cloned().collect();
        names.sort_unstable();
        let mut fields = HashMap::with_capacity(names.len());
        for name in names {
            let Some(def) = defs.remove(&name) else {
                continue;
            };
            let handle = match def.kind {
                Kind::Text => builder.add_text_field(
                    &name,
                    text_options("whitespace", IndexRecordOption::WithFreqsAndPositions),
                ),
                Kind::Int | Kind::Float => {
                    builder.add_f64_field(&name, NumericOptions::default().set_indexed().set_fast())
                }
                _ => builder.add_text_field(&name, text_options("raw", IndexRecordOption::Basic)),
            };
            let (index_analyzer, query_analyzer) = schema.field_type(&def).map_or_else(
                || (Arc::new(Analyzer::raw()), Arc::new(Analyzer::raw())),
                |t| (Arc::clone(&t.index), Arc::clone(&t.query)),
            );
            fields.insert(
                name,
                Indexed {
                    def,
                    handle,
                    index_analyzer,
                    query_analyzer,
                },
            );
        }
        let index = tantivy::Index::create_in_ram(builder.build());
        let mut terms = BTreeSet::new();
        {
            let mut writer = index.writer_with_num_threads::<TantivyDocument>(1, 15_000_000)?;
            for (i, doc) in docs.iter().enumerate() {
                let row = schema.effective(object(doc, &empty));
                add_spelling_terms(&row, &fields, &mut terms);
                writer.add_document(to_tantivy(&row, i as u64, ord, &fields))?;
            }
            writer.commit()?;
        }
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let by_key = docs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| Some((d.get(&schema.unique_key)?.as_str()?.to_owned(), i)))
            .collect();
        Ok(Self {
            schema,
            term_dict: terms.into_iter().collect(),
            docs,
            version,
            reader,
            fields,
            by_key,
            facet_columns: Mutex::new(HashMap::new()),
        })
    }

    /// The facet column of `field`, built on first use and kept with this snapshot.
    pub fn facet_column(&self, field: &str, keying: Keying) -> Arc<FacetColumn> {
        let mut cache = self
            .facet_columns
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(cache.entry((field.to_owned(), keying)).or_insert_with(|| {
            Arc::new(FacetColumn::build(&self.schema, &self.docs, field, keying))
        }))
    }

    /// The indexed field for a query on `name`.
    ///
    /// `Ok(None)` means the schema knows the field but no document has it, so nothing
    /// matches.
    ///
    /// # Errors
    /// [`Error::Unsupported`] for a name the schema does not define or cannot search.
    pub fn resolve(&self, name: &str) -> Result<Option<&Indexed>, Error> {
        if let Some(f) = self.fields.get(name) {
            return Ok(Some(f));
        }
        match self.schema.get(name) {
            None => Err(Error::Unsupported(format!("undefined field {name}"))),
            Some(f) if !f.searchable() => Err(Error::Unsupported(format!(
                "field {name} is neither indexed nor docValues"
            ))),
            Some(f) if !f.in_index() => Err(Error::Unsupported(format!(
                "query on {:?} field {name}",
                f.kind
            ))),
            Some(_) => Ok(None),
        }
    }

    /// The row at `ord`, or `null` when there is none.
    #[must_use]
    pub fn row(&self, ord: usize) -> &Value {
        static NULL: Value = Value::Null;
        self.docs.get(ord).map_or(&NULL, |d| d)
    }

    /// The row whose uniqueKey is `key`.
    #[must_use]
    pub fn ord_of_key(&self, key: &str) -> Option<usize> {
        self.by_key.get(key).copied()
    }

    /// Names of the fields in the Tantivy index.
    pub fn indexed_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }
}

fn object<'a>(doc: &'a Value, empty: &'a Map<String, Value>) -> &'a Map<String, Value> {
    doc.as_object().unwrap_or(empty)
}

/// Adds a row's words to the spellcheck dictionary, as Python's `_index_value` does for
/// the indexed text fields `title`, `author_name` and `name`.
fn add_spelling_terms(
    row: &Map<String, Value>,
    fields: &HashMap<String, Indexed>,
    terms: &mut BTreeSet<String>,
) {
    for name in ["title", "author_name", "name"] {
        let (Some(raw), Some(f)) = (row.get(name), fields.get(name)) else {
            continue;
        };
        if f.def.kind != Kind::Text {
            continue;
        }
        for v in values(raw).filter(|v| !v.is_null()) {
            let text = crate::query::py_str(v);
            for word in crate::unicode::split_whitespace(&text) {
                if word.chars().count() > 3 {
                    terms.insert(fold(word));
                }
            }
        }
    }
}

fn text_options(tokenizer: &str, option: IndexRecordOption) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(option),
    )
}

/// The values of a field: the items of an array, or the value itself.
pub(crate) fn values(v: &Value) -> impl Iterator<Item = &Value> {
    match v {
        Value::Array(items) => items.iter(),
        other => std::slice::from_ref(other).iter(),
    }
}

fn to_tantivy(
    row: &Map<String, Value>,
    ord_value: u64,
    ord: Field,
    fields: &HashMap<String, Indexed>,
) -> TantivyDocument {
    let mut out = TantivyDocument::default();
    out.add_u64(ord, ord_value);
    for (name, raw) in row {
        let Some(Indexed {
            def,
            handle,
            index_analyzer,
            ..
        }) = fields.get(name)
        else {
            continue;
        };
        let field = *handle;
        for v in values(raw) {
            match (def.kind, v) {
                (_, Value::Null) => {}
                (Kind::Text, _) => {
                    let analysis = index_analyzer.index(&crate::query::py_str(v));
                    if !analysis.main.is_empty() {
                        out.add_text(field, analysis.main.join(" "));
                    }
                    if !analysis.extra.is_empty() {
                        out.add_text(field, analysis.extra.join(" "));
                    }
                }
                (Kind::Int | Kind::Float, Value::Number(n)) => {
                    if let Some(x) = n.as_f64() {
                        out.add_f64(field, x);
                    }
                }
                (Kind::Int | Kind::Float, _) => {}
                (_, Value::Bool(b)) => out.add_text(field, if *b { "true" } else { "false" }),
                _ => out.add_text(field, as_text(v)),
            }
        }
    }
    out
}

/// A JSON scalar as the text Solr would index for it.
pub(crate) fn as_text(v: &Value) -> std::borrow::Cow<'_, str> {
    match v {
        Value::String(s) => s.as_str().into(),
        other => other.to_string().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<schema>
  <fieldType name="string" class="solr.StrField"/>
  <fieldType name="pint" class="solr.IntPointField"/>
  <fieldType name="text_general" class="solr.TextField">
    <analyzer><tokenizer name="standard"/><filter name="lowercase"/></analyzer>
  </fieldType>
  <field name="key" type="string"/>
  <field name="title" type="text_general"/>
  <field name="title_suggest" type="text_general"/>
  <field name="edition_count" type="pint"/>
  <dynamicField name="*_s" type="string"/>
  <dynamicField name="*_i" type="pint"/>
  <copyField source="title" dest="title_suggest"/>
</schema>"#;

    #[test]
    fn indexes_static_and_seen_dynamic_fields() {
        let schema = Schema::parse(XML, None).expect("schema");
        let docs = vec![
            serde_json::json!({"key": "/works/OL1W", "title": "Alice in Wonderland", "edition_count": 3, "lending_s": "x"}),
        ];
        let docs = docs.into_iter().map(Arc::new).collect();
        let store = Index::from_docs(Arc::new(schema), docs, 0).expect("index builds");
        assert_eq!(store.reader.searcher().num_docs(), 1);
        let mut names: Vec<&str> = store.indexed_names().collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "edition_count",
                "key",
                "lending_s",
                "title",
                "title_suggest"
            ]
        );
        assert!(store.resolve("count_i").expect("known dynamic").is_none());
        assert!(matches!(store.resolve("nope"), Err(Error::Unsupported(_))));
    }
}
