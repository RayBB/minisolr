//! The document store and the RAM Tantivy index built from the parsed schema.
//!
//! Text fields are analyzed here and indexed as space-joined tokens on Tantivy's
//! `whitespace` tokenizer; strings, enums, dates and booleans on `raw`; numbers as `f64`.
//! Like Python's `Store._rebuild`, every static field that is searchable is in the index,
//! plus each dynamic field name that occurs in a document. copyField values are applied
//! at index time (and again when a document is returned); the stored documents stay as
//! loaded. Nested children are separate rows sharing `_root_`. A fast `ord` field maps a
//! hit back to the document held in memory.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::{Map, Value};
use tantivy::schema::{Field, IndexRecordOption, NumericOptions, TextFieldIndexing, TextOptions};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument};

use crate::Error;
use crate::analysis::Analyzer;
use crate::schema::{self, Kind, Schema};

/// A schema field that is in the Tantivy index.
#[derive(Debug)]
pub struct Indexed {
    /// The schema definition (concrete name for dynamic fields).
    pub def: schema::Field,
    /// The Tantivy field handle.
    pub handle: Field,
}

/// The documents and their index.
pub struct Store {
    /// The schema the index was built from.
    pub schema: Schema,
    /// The analyzer shared by indexing and querying.
    pub analyzer: Analyzer,
    /// Documents in `ord` order, as loaded.
    pub docs: Vec<Value>,
    pub(crate) reader: IndexReader,
    pub(crate) fields: HashMap<String, Indexed>,
}

impl Store {
    /// Loads a JSON-lines file of documents and indexes them in RAM.
    ///
    /// # Errors
    /// Fails if the file cannot be read or parsed, or if Tantivy fails.
    pub fn load(schema: Schema, path: &Path) -> Result<Self, Error> {
        let mut docs = Vec::new();
        for line in BufReader::new(std::fs::File::open(path)?).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                docs.push(serde_json::from_str::<Value>(&line)?);
            }
        }
        Self::from_docs(schema, docs)
    }

    /// Indexes already parsed documents in RAM.
    ///
    /// # Errors
    /// Fails if Tantivy fails.
    pub fn from_docs(schema: Schema, docs: Vec<Value>) -> Result<Self, Error> {
        let empty = Map::new();
        let as_map = |d| object(d, &empty);
        let mut defs: HashMap<String, schema::Field> = schema
            .fields
            .values()
            .filter(|f| f.in_index())
            .map(|f| (f.name.clone(), f.clone()))
            .collect();
        for doc in &docs {
            for name in schema.effective(as_map(doc)).keys() {
                if !defs.contains_key(name)
                    && !schema.fields.contains_key(name)
                    && let Some(f) = schema.get(name).filter(|f| f.in_index())
                {
                    defs.insert(name.clone(), f.into_owned());
                }
            }
        }
        warn_fallback_chains(&schema, defs.values());

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
            fields.insert(name, Indexed { def, handle });
        }
        let index = Index::create_in_ram(builder.build());
        let analyzer = Analyzer::new();
        {
            let mut writer = index.writer_with_num_threads::<TantivyDocument>(1, 15_000_000)?;
            for (i, doc) in docs.iter().enumerate() {
                let row = schema.effective(as_map(doc));
                writer.add_document(to_tantivy(&row, i as u64, ord, &fields, analyzer))?;
            }
            writer.commit()?;
        }
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            schema,
            analyzer,
            docs,
            reader,
            fields,
        })
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

    /// Names of the fields in the Tantivy index.
    pub fn indexed_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }
}

fn object<'a>(doc: &'a Value, empty: &'a Map<String, Value>) -> &'a Map<String, Value> {
    doc.as_object().unwrap_or(empty)
}

fn warn_fallback_chains<'a>(schema: &Schema, defs: impl Iterator<Item = &'a schema::Field>) {
    let mut warned = HashSet::new();
    for def in defs {
        if def.kind == Kind::Text
            && let Some(t) = schema.field_type(def)
            && !t.chain_implemented
            && warned.insert(t.name.clone())
        {
            eprintln!(
                "warning: field type {} has no implemented analyzer; using whitespace + lowercase (first field: {})",
                t.name, def.name
            );
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
    analyzer: Analyzer,
) -> TantivyDocument {
    let mut out = TantivyDocument::default();
    out.add_u64(ord, ord_value);
    for (name, raw) in row {
        let Some(Indexed { def, handle }) = fields.get(name) else {
            continue;
        };
        let field = *handle;
        for v in values(raw) {
            match (def.kind, v) {
                (_, Value::Null) => {}
                (Kind::Text, _) => {
                    let analysis = analyzer.index(def.chain, &as_text(v));
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
        let store = Store::from_docs(schema, docs).expect("index builds");
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
