//! The hard-coded field table, the document store and the RAM Tantivy index.
//!
//! Text fields are analyzed here and indexed as space-joined tokens on Tantivy's
//! `whitespace` tokenizer, strings on `raw`, numbers as `f64`. A fast `ord` field maps a
//! hit back to the document held in memory. The table is a small subset of
//! `managed-schema.xml`; it is not parsed from the XML.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument};

use crate::Error;
use crate::analysis::{Analyzer, Chain};

/// How a field is indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Analyzed text.
    Text(Chain),
    /// Exact-match string.
    Str,
    /// JSON boolean, indexed as the strings `true` and `false`.
    Bool,
    /// Number, indexed as `f64`.
    Num,
}

/// One entry of the field table.
#[derive(Debug, PartialEq)]
pub struct FieldDef {
    /// Solr field name.
    pub name: &'static str,
    /// Indexing kind.
    pub kind: Kind,
    /// Document key to read values from when it differs from `name` (a `copyField`).
    pub source: Option<&'static str>,
}

const fn def(name: &'static str, kind: Kind) -> FieldDef {
    FieldDef {
        name,
        kind,
        source: None,
    }
}

const fn copy(name: &'static str, source: &'static str, kind: Kind) -> FieldDef {
    FieldDef {
        name,
        kind,
        source: Some(source),
    }
}

const EN: Kind = Kind::Text(Chain::EnSplitting);
const INTL: Kind = Kind::Text(Chain::International);
const GENERAL: Kind = Kind::Text(Chain::General);

/// The fields this prototype can search. Hard-coded.
pub const FIELDS: &[FieldDef] = &[
    def("title", EN),
    def("alternative_title", EN),
    def("subject", EN),
    def("publisher", EN),
    def("series_name", EN),
    def("chapter", EN),
    def("time", EN),
    def("oclc", EN),
    def("author_name", INTL),
    def("author_alternative_name", INTL),
    def("name", INTL),
    def("alternate_names", INTL),
    def("place", GENERAL),
    def("person", GENERAL),
    def("contributor", GENERAL),
    def("lccn", GENERAL),
    def("key", Kind::Str),
    def("type", Kind::Str),
    def("author_key", Kind::Str),
    def("edition_key", Kind::Str),
    def("ia", Kind::Str),
    def("isbn", Kind::Str),
    def("language", Kind::Str),
    def("ebook_access", Kind::Str),
    def("series_key", Kind::Str),
    def("series_position", Kind::Str),
    def("subject_key", Kind::Str),
    def("place_key", Kind::Str),
    def("person_key", Kind::Str),
    def("time_key", Kind::Str),
    copy("subject_facet", "subject", Kind::Str),
    copy("publisher_facet", "publisher", Kind::Str),
    def("has_fulltext", Kind::Bool),
    def("public_scan_b", Kind::Bool),
    def("edition_count", Kind::Num),
    def("first_publish_year", Kind::Num),
    def("readinglog_count", Kind::Num),
    def("work_count", Kind::Num),
];

/// Looks a field up by name.
#[must_use]
pub fn field_def(name: &str) -> Option<&'static FieldDef> {
    FIELDS.iter().find(|f| f.name == name)
}

/// The documents and their index.
pub struct Store {
    /// The analyzer shared by indexing and querying.
    pub analyzer: Analyzer,
    /// Documents in `ord` order, as loaded.
    pub docs: Vec<Value>,
    pub(crate) reader: IndexReader,
    pub(crate) fields: HashMap<&'static str, Field>,
}

impl Store {
    /// Loads a JSON-lines file of documents and indexes them in RAM.
    ///
    /// # Errors
    /// Fails if the file cannot be read or parsed, or if Tantivy fails.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let mut docs = Vec::new();
        for line in BufReader::new(std::fs::File::open(path)?).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                docs.push(serde_json::from_str::<Value>(&line)?);
            }
        }
        Self::from_docs(docs)
    }

    /// Indexes already parsed documents in RAM.
    ///
    /// # Errors
    /// Fails if Tantivy fails.
    pub fn from_docs(docs: Vec<Value>) -> Result<Self, Error> {
        let mut builder = Schema::builder();
        let ord = builder.add_u64_field("ord", NumericOptions::default().set_fast());
        let mut fields = HashMap::new();
        for f in FIELDS {
            let field = match f.kind {
                Kind::Text(_) => builder.add_text_field(
                    f.name,
                    text_options("whitespace", IndexRecordOption::WithFreqsAndPositions),
                ),
                Kind::Str | Kind::Bool => {
                    builder.add_text_field(f.name, text_options("raw", IndexRecordOption::Basic))
                }
                Kind::Num => builder
                    .add_f64_field(f.name, NumericOptions::default().set_indexed().set_fast()),
            };
            fields.insert(f.name, field);
        }
        let index = Index::create_in_ram(builder.build());
        let analyzer = Analyzer::new();
        {
            let mut writer = index.writer_with_num_threads::<TantivyDocument>(1, 15_000_000)?;
            for (i, doc) in docs.iter().enumerate() {
                writer.add_document(to_tantivy(doc, i as u64, ord, &fields, analyzer))?;
            }
            writer.commit()?;
        }
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            analyzer,
            docs,
            reader,
            fields,
        })
    }

    /// Tantivy handle for a field in the table.
    pub(crate) fn field(&self, name: &str) -> Option<Field> {
        self.fields.get(name).copied()
    }
}

fn text_options(tokenizer: &str, option: IndexRecordOption) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(option),
    )
}

fn values(v: &Value) -> impl Iterator<Item = &Value> {
    match v {
        Value::Array(items) => items.iter(),
        other => std::slice::from_ref(other).iter(),
    }
}

fn to_tantivy(
    doc: &Value,
    ord_value: u64,
    ord: Field,
    fields: &HashMap<&'static str, Field>,
    analyzer: Analyzer,
) -> TantivyDocument {
    let mut out = TantivyDocument::default();
    out.add_u64(ord, ord_value);
    for f in FIELDS {
        let (Some(&field), Some(raw)) = (fields.get(f.name), doc.get(f.source.unwrap_or(f.name)))
        else {
            continue;
        };
        for v in values(raw) {
            match (f.kind, v) {
                (_, Value::Null) => {}
                (Kind::Text(chain), _) => {
                    let analysis = analyzer.index(chain, &as_text(v));
                    if !analysis.main.is_empty() {
                        out.add_text(field, analysis.main.join(" "));
                    }
                    if !analysis.extra.is_empty() {
                        out.add_text(field, analysis.extra.join(" "));
                    }
                }
                (Kind::Str, _) => out.add_text(field, as_text(v)),
                (Kind::Bool, Value::Bool(b)) => {
                    out.add_text(field, if *b { "true" } else { "false" });
                }
                (Kind::Num, Value::Number(n)) => {
                    if let Some(x) = n.as_f64() {
                        out.add_f64(field, x);
                    }
                }
                _ => {}
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

    #[test]
    fn field_table_has_unique_names() {
        let mut names: Vec<_> = FIELDS.iter().map(|f| f.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), FIELDS.len());
        assert!(field_def("title").is_some());
        assert!(field_def("nope").is_none());
    }

    #[test]
    fn builds_an_index_from_documents() {
        let docs = vec![
            serde_json::json!({"key": "/works/OL1W", "type": "work", "title": "Alice in Wonderland", "edition_count": 3}),
        ];
        let store = Store::from_docs(docs).expect("index builds");
        assert_eq!(store.reader.searcher().num_docs(), 1);
    }
}
