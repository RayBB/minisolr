//! Solr schema loaded from `managed-schema.xml` and `enumsConfig.xml`.
//!
//! Mirrors the Python `minisolr.schema`: field types (class, attribute defaults, analyzers,
//! enum values), fields, dynamic fields matched longest pattern first, copyFields and the
//! uniqueKey. Like the Python code, only direct children of `<schema>` are read.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::Error;
use crate::analysis::{Analyzer, Component, Resources};

/// What a field holds, derived from its field type class (same names as `schema.py`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Analyzed text (`TextField`, `SortableTextField`).
    Text,
    /// Exact string; also any class not listed below.
    Str,
    /// Integer point field.
    Int,
    /// Floating point field.
    Float,
    /// Boolean.
    Bool,
    /// Enum with a configured value order.
    Enum,
    /// Date point field (kept as text for now).
    Date,
    /// ICU collation sort key; not searchable.
    SortText,
    /// Spatial, random, nest-path and other fields that are never indexed.
    Skip,
}

impl Kind {
    /// The kind for a field type class such as `solr.TextField`.
    #[must_use]
    pub fn from_class(class: &str) -> Self {
        match class {
            "solr.IntPointField" | "solr.LongPointField" => Self::Int,
            "solr.FloatPointField" | "solr.DoublePointField" => Self::Float,
            "solr.TextField" | "solr.SortableTextField" => Self::Text,
            "solr.ICUCollationField" => Self::SortText,
            "solr.BoolField" => Self::Bool,
            "solr.EnumFieldType" => Self::Enum,
            "solr.DatePointField" => Self::Date,
            "solr.NestPathField"
            | "solr.RandomSortField"
            | "solr.LatLonPointSpatialField"
            | "solr.SpatialRecursivePrefixTreeFieldType"
            | "solr.PointType"
            | "solr.RankField"
            | "solr.BinaryField" => Self::Skip,
            _ => Self::Str,
        }
    }

    /// Whether Solr can serve this kind from docValues (queries, `fl=*`).
    #[must_use]
    pub fn doc_values_capable(self) -> bool {
        matches!(
            self,
            Self::Int | Self::Float | Self::Str | Self::Bool | Self::Enum | Self::Date
        )
    }

    /// Whether values are numbers indexed as `f64`.
    #[must_use]
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Int | Self::Float)
    }
}

/// An analyzer element: its tokenizer and filters in chain order, with lowercased Solr
/// SPI names (Lucene looks SPI names up case-insensitively).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalyzerSpec {
    /// The tokenizer, such as `whitespace` or `standard`.
    pub tokenizer: Component,
    /// The filters, such as `icufolding`.
    pub filters: Vec<Component>,
}

impl AnalyzerSpec {
    /// The filter names, in order.
    #[must_use]
    pub fn filter_names(&self) -> Vec<&str> {
        self.filters.iter().map(|f| f.name.as_str()).collect()
    }
}

/// A `<fieldType>`.
#[derive(Clone, Debug)]
pub struct FieldType {
    /// Type name.
    pub name: String,
    /// Implementation class, such as `solr.TextField`.
    pub class: String,
    /// Kind derived from the class.
    pub kind: Kind,
    /// Index-time analyzer (also used for queries when there is no query analyzer).
    pub index_analyzer: Option<AnalyzerSpec>,
    /// Query-time analyzer.
    pub query_analyzer: Option<AnalyzerSpec>,
    /// The index-time chain (one token per value when the type has no index analyzer).
    pub index: Arc<Analyzer>,
    /// The query-time chain (the whole text as one token when there is no query analyzer,
    /// as in Python, which does not fall back to the index analyzer).
    pub query: Arc<Analyzer>,
    /// Tokenizers and filters of either analyzer that are not implemented (skipped).
    pub skipped: Vec<String>,
    /// Enum values in sort order (`EnumFieldType` only).
    pub enum_values: Vec<String>,
    attrs: HashMap<String, String>,
}

impl FieldType {
    /// A boolean attribute of the type, or `default` when absent.
    #[must_use]
    pub fn attr_true(&self, name: &str, default: bool) -> bool {
        self.attrs
            .get(name)
            .map_or(default, |v| v.eq_ignore_ascii_case("true"))
    }
}

/// A `<field>` or a concrete name resolved through a `<dynamicField>`.
// The four flags are Solr's independent field attributes; enums would only obscure that.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Field name (the concrete name for a dynamic match).
    pub name: String,
    /// Field type name.
    pub type_name: String,
    /// Kind of the field type.
    pub kind: Kind,
    /// `indexed` attribute after defaults.
    pub indexed: bool,
    /// `stored` attribute after defaults.
    pub stored: bool,
    /// `multiValued` attribute after defaults.
    pub multi: bool,
    /// `docValues` attribute after defaults.
    pub doc_values: bool,
}

impl Field {
    /// Whether queries can match the field: indexed, or docValues of a capable kind.
    #[must_use]
    pub fn searchable(&self) -> bool {
        self.indexed || (self.doc_values && self.kind.doc_values_capable())
    }

    /// Whether `fl=*` returns the field: stored, or docValues of a capable kind.
    #[must_use]
    pub fn returned_in_star(&self) -> bool {
        self.stored || (self.doc_values && self.kind.doc_values_capable())
    }

    /// Whether the store puts the field in the Tantivy index.
    #[must_use]
    pub fn in_index(&self) -> bool {
        self.searchable() && !matches!(self.kind, Kind::Skip | Kind::SortText)
    }
}

/// The parsed schema.
#[derive(Clone, Debug)]
pub struct Schema {
    /// Field types by name.
    pub types: HashMap<String, FieldType>,
    /// Static fields by name.
    pub fields: HashMap<String, Field>,
    /// Dynamic fields, longest pattern first (file order among equal lengths).
    pub dynamic: Vec<Field>,
    /// `(source, dest)` copyField pairs in file order; a source may be a `*` glob.
    pub copy_fields: Vec<(String, String)>,
    /// The uniqueKey field name.
    pub unique_key: String,
}

impl Schema {
    /// Reads `managed-schema.xml` and, if present, `enumsConfig.xml` from `conf_dir`.
    ///
    /// # Errors
    /// Fails if the schema cannot be read or is invalid.
    pub fn load(conf_dir: &Path) -> Result<Self, Error> {
        let schema = std::fs::read_to_string(conf_dir.join("managed-schema.xml"))?;
        let enums_path = conf_dir.join("enumsConfig.xml");
        let enums = if enums_path.is_file() {
            Some(std::fs::read_to_string(enums_path)?)
        } else {
            None
        };
        Self::parse_with(&schema, enums.as_deref(), &Resources::new(Some(conf_dir)))
    }

    /// Parses schema XML and optional enums XML, with no word list files.
    ///
    /// # Errors
    /// Fails on malformed XML or a field whose type is not defined.
    pub fn parse(schema_xml: &str, enums_xml: Option<&str>) -> Result<Self, Error> {
        Self::parse_with(schema_xml, enums_xml, &Resources::default())
    }

    /// Parses schema XML and optional enums XML; analyzers read word lists from `res`.
    ///
    /// # Errors
    /// Fails on malformed XML or a field whose type is not defined.
    pub fn parse_with(
        schema_xml: &str,
        enums_xml: Option<&str>,
        res: &Resources,
    ) -> Result<Self, Error> {
        let enums = enums_xml.map(parse_enums).transpose()?.unwrap_or_default();
        let doc = roxmltree::Document::parse(schema_xml)?;
        let root = doc.root_element();
        let mut types = HashMap::new();
        for el in children(root, "fieldType") {
            let t = field_type(el, &enums, res);
            types.insert(t.name.clone(), t);
        }
        let mut fields = HashMap::new();
        for el in children(root, "field") {
            let f = field(el, &types)?;
            fields.insert(f.name.clone(), f);
        }
        let mut dynamic = children(root, "dynamicField")
            .map(|el| field(el, &types))
            .collect::<Result<Vec<_>, _>>()?;
        dynamic.sort_by_key(|d| std::cmp::Reverse(d.name.len()));
        let copy_fields = children(root, "copyField")
            .map(|el| (attr(el, "source").to_owned(), attr(el, "dest").to_owned()))
            .collect();
        let unique_key = children(root, "uniqueKey")
            .next()
            .and_then(|el| el.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("key")
            .to_owned();
        Ok(Self {
            types,
            fields,
            dynamic,
            copy_fields,
            unique_key,
        })
    }

    /// The field for a concrete name, static or through the first matching dynamic field.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Cow<'_, Field>> {
        if let Some(f) = self.fields.get(name) {
            return Some(Cow::Borrowed(f));
        }
        self.dynamic
            .iter()
            .find(|d| glob_match(&d.name, name))
            .map(|d| {
                Cow::Owned(Field {
                    name: name.to_owned(),
                    ..d.clone()
                })
            })
    }

    /// The type of a field.
    #[must_use]
    pub fn field_type(&self, field: &Field) -> Option<&FieldType> {
        self.types.get(&field.type_name)
    }

    /// One line per text field type that a field or dynamic field uses and whose
    /// analyzers have components with no implementation, naming the type and the
    /// components (skipped filters; tokenizers replaced by the standard one).
    #[must_use]
    pub fn analysis_warnings(&self) -> Vec<String> {
        let used: std::collections::BTreeSet<&str> = self
            .fields
            .values()
            .chain(&self.dynamic)
            .filter(|f| f.kind == Kind::Text)
            .map(|f| f.type_name.as_str())
            .collect();
        used.into_iter()
            .filter_map(|name| self.types.get(name))
            .filter(|t| !t.skipped.is_empty())
            .map(|t| format!("{}: {}", t.name, t.skipped.join(", ")))
            .collect()
    }

    /// The document as search sees it: copyField destinations get the source values
    /// appended, like Python's `Store._effective`. Returns the input when nothing is copied.
    #[must_use]
    pub fn effective<'a>(&self, doc: &'a Map<String, Value>) -> Cow<'a, Map<String, Value>> {
        let mut out: Option<Map<String, Value>> = None;
        for (src, dst) in &self.copy_fields {
            let sources: Vec<&Value> = if src.contains('*') {
                doc.iter()
                    .filter(|(k, _)| glob_match(src, k))
                    .map(|(_, v)| v)
                    .collect()
            } else {
                doc.get(src).into_iter().collect()
            };
            for value in sources {
                let eff = out.get_or_insert_with(|| doc.clone());
                let mut list = match eff.remove(dst) {
                    Some(Value::Array(items)) => items,
                    Some(other) => vec![other],
                    None => Vec::new(),
                };
                match value {
                    Value::Array(items) => list.extend(items.iter().cloned()),
                    other => list.push(other.clone()),
                }
                eff.insert(dst.clone(), Value::Array(list));
            }
        }
        out.map_or(Cow::Borrowed(doc), Cow::Owned)
    }

    /// The values `name` has in the copyField-expanded row ([`Schema::effective`]),
    /// flattened, without building that row: the document's own values, then each
    /// copyField source's in rule order. `None` when the expanded row has no `name`.
    /// A top-level `null` contributes no value.
    #[must_use]
    pub fn values_of<'a>(&self, doc: &'a Map<String, Value>, name: &str) -> Option<Vec<&'a Value>> {
        fn push<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
            match v {
                Value::Array(items) => out.extend(items),
                Value::Null => {}
                other => out.push(other),
            }
        }
        let mut present = false;
        let mut out = Vec::new();
        if let Some(v) = doc.get(name) {
            present = true;
            push(v, &mut out);
        }
        for (src, _) in self.copy_fields.iter().filter(|(_, dst)| dst == name) {
            if src.contains('*') {
                for (_, v) in doc.iter().filter(|(k, _)| glob_match(src, k)) {
                    present = true;
                    push(v, &mut out);
                }
            } else if let Some(v) = doc.get(src) {
                present = true;
                push(v, &mut out);
            }
        }
        present.then_some(out)
    }
}

/// Matches `name` against a pattern where `*` stands for any run of characters.
#[must_use]
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return name.is_empty();
    };
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let mut middle: Vec<&str> = parts.collect();
    let Some(last) = middle.pop() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = rest.get(at + part.len()..).unwrap_or_default(),
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

fn children<'a, 'input>(
    el: roxmltree::Node<'a, 'input>,
    tag: &'static str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    el.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == tag)
}

fn attr<'a>(el: roxmltree::Node<'a, '_>, name: &str) -> &'a str {
    el.attribute(name).unwrap_or_default()
}

fn parse_enums(xml: &str) -> Result<HashMap<String, Vec<String>>, Error> {
    let doc = roxmltree::Document::parse(xml)?;
    Ok(children(doc.root_element(), "enum")
        .map(|e| {
            let values = children(e, "value")
                .map(|v| v.text().unwrap_or_default().trim().to_owned())
                .collect();
            (attr(e, "name").to_owned(), values)
        })
        .collect())
}

fn field_type(
    el: roxmltree::Node<'_, '_>,
    enums: &HashMap<String, Vec<String>>,
    res: &Resources,
) -> FieldType {
    let class = attr(el, "class").to_owned();
    let kind = Kind::from_class(&class);
    let mut index_analyzer = None;
    let mut query_analyzer = None;
    let (mut index, mut query) = (Arc::new(Analyzer::raw()), Arc::new(Analyzer::raw()));
    let mut skipped: Vec<String> = Vec::new();
    for a in children(el, "analyzer") {
        let spec = analyzer(a);
        let (built, missing) = Analyzer::build(&spec.tokenizer, &spec.filters, res);
        for m in missing {
            if !skipped.contains(&m) {
                skipped.push(m);
            }
        }
        let built = Arc::new(built);
        let which = a.attribute("type");
        if matches!(which, None | Some("index")) {
            index_analyzer = Some(spec.clone());
            index = Arc::clone(&built);
        }
        if matches!(which, None | Some("query")) {
            query_analyzer = Some(spec);
            query = built;
        }
    }
    let enum_values = if kind == Kind::Enum {
        enums.get(attr(el, "enumName")).cloned().unwrap_or_default()
    } else {
        Vec::new()
    };
    FieldType {
        name: attr(el, "name").to_owned(),
        class,
        kind,
        index_analyzer,
        query_analyzer,
        index,
        query,
        skipped,
        enum_values,
        attrs: el
            .attributes()
            .map(|a| (a.name().to_owned(), a.value().to_owned()))
            .collect(),
    }
}

fn analyzer(el: roxmltree::Node<'_, '_>) -> AnalyzerSpec {
    let component = |c: roxmltree::Node<'_, '_>, suffix: &str| Component {
        name: spi_name(c, suffix),
        attrs: c
            .attributes()
            .map(|a| (a.name().to_owned(), a.value().to_owned()))
            .collect(),
    };
    AnalyzerSpec {
        tokenizer: children(el, "tokenizer")
            .next()
            .map(|t| component(t, "TokenizerFactory"))
            .unwrap_or_default(),
        filters: children(el, "filter")
            .map(|f| component(f, "FilterFactory"))
            .collect(),
    }
}

/// The lowercased SPI name of a tokenizer or filter, from `name` or from a `class` such
/// as `solr.WhitespaceTokenizerFactory` (giving `whitespace`).
fn spi_name(el: roxmltree::Node<'_, '_>, suffix: &str) -> String {
    if let Some(name) = el.attribute("name") {
        return name.to_lowercase();
    }
    let class = attr(el, "class");
    let short = class.rsplit('.').next().unwrap_or(class);
    short.strip_suffix(suffix).unwrap_or(short).to_lowercase()
}

fn field(el: roxmltree::Node<'_, '_>, types: &HashMap<String, FieldType>) -> Result<Field, Error> {
    let name = attr(el, "name");
    let type_name = attr(el, "type");
    let t = types.get(type_name).ok_or_else(|| {
        Error::Schema(format!(
            "field {name} has undefined field type {type_name:?}"
        ))
    })?;
    let flag = |attr_name: &str, default: bool| {
        el.attribute(attr_name).map_or_else(
            || t.attr_true(attr_name, default),
            |v| v.eq_ignore_ascii_case("true"),
        )
    };
    Ok(Field {
        name: name.to_owned(),
        type_name: type_name.to_owned(),
        kind: t.kind,
        indexed: flag("indexed", true),
        stored: flag("stored", true),
        multi: flag("multiValued", false),
        doc_values: flag("docValues", t.kind.doc_values_capable()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const XML: &str = r#"<?xml version="1.0"?>
<schema name="t" version="1.6">
  <!-- a comment -->
  <uniqueKey>key</uniqueKey>
  <fieldType name="string" class="solr.StrField" sortMissingLast="true"/>
  <fieldType name="strings" class="solr.StrField" multiValued="true"/>
  <fieldType name="pint" class="solr.IntPointField"/>
  <fieldType name="nodv" class="solr.StrField" docValues="false"/>
  <fieldType name="sort" class="solr.ICUCollationField"/>
  <fieldType name="access" class="solr.EnumFieldType" enumsConfig="enumsConfig.xml" enumName="ebookAccess"/>
  <fieldType name="text_en_splitting" class="solr.TextField" autoGeneratePhraseQueries="true">
    <analyzer type="index">
      <tokenizer name="whitespace"/>
      <filter name="icuFolding"/>
      <filter name="wordDelimiterGraph" generateWordParts="1"/>
      <filter name="lowercase"/>
      <filter name="keywordMarker" protected="protwords.txt"/>
      <filter name="porterStem"/>
      <filter name="flattenGraph"/>
    </analyzer>
    <analyzer type="query">
      <tokenizer name="whitespace"/>
      <filter name="synonymGraph"/>
      <filter name="icuFolding"/>
    </analyzer>
  </fieldType>
  <fieldType name="text_general" class="solr.TextField" multiValued="true">
    <analyzer>
      <tokenizer class="solr.StandardTokenizerFactory"/>
      <filter class="solr.StopFilterFactory" words="stopwords.txt"/>
      <filter class="solr.LowerCaseFilterFactory"/>
    </analyzer>
  </fieldType>
  <fieldType name="text_de" class="solr.TextField">
    <analyzer><tokenizer name="standard"/><filter name="germanLightStem"/></analyzer>
  </fieldType>
  <field name="key" type="string" required="true"/>
  <field name="title" type="text_en_splitting"/>
  <field name="title_suggest" type="text_general" multiValued="false"/>
  <field name="subject" type="text_en_splitting" multiValued="true" stored="false"/>
  <field name="subject_facet" type="string" indexed="false" stored="false" multiValued="true"/>
  <field name="cover_edition_key" type="nodv" indexed="false"/>
  <field name="edition_count" type="pint"/>
  <field name="ebook_access" type="access"/>
  <field name="title_sort" type="sort"/>
  <dynamicField name="*_s" type="string" indexed="true" stored="true"/>
  <dynamicField name="*_ss" type="strings"/>
  <dynamicField name="id_*" type="string" multiValued="true"/>
  <dynamicField name="*_txt_de" type="text_de"/>
  <dynamicField name="*" type="nodv" indexed="false" stored="false"/>
  <copyField source="title" dest="title_suggest"/>
  <copyField source="subject" dest="subject_facet"/>
  <copyField source="*_s" dest="all_ss"/>
</schema>"#;

    const ENUMS: &str = r#"<?xml version="1.0"?>
<enumsConfig>
  <enum name="ebookAccess"><value>no_ebook</value><value> borrowable </value><value>public</value></enum>
</enumsConfig>"#;

    fn schema() -> Schema {
        Schema::parse(XML, Some(ENUMS)).expect("schema parses")
    }

    #[test]
    fn defaults_follow_type_then_field_attributes() {
        let s = schema();
        let key = s.get("key").expect("key");
        assert_eq!(key.kind, Kind::Str);
        assert!(key.indexed && key.stored && !key.multi && key.doc_values);
        let suggest = s.get("title_suggest").expect("suggest");
        assert!(
            !suggest.multi,
            "field attribute overrides the type's multiValued"
        );
        let subject = s.get("subject").expect("subject");
        assert!(subject.multi && !subject.stored && !subject.doc_values);
        let facet = s.get("subject_facet").expect("facet");
        assert!(facet.searchable() && facet.returned_in_star() && facet.in_index());
        let cover = s.get("cover_edition_key").expect("cover");
        assert!(!cover.doc_values, "type attribute docValues=false applies");
        assert!(!cover.searchable() && cover.returned_in_star());
        let sort = s.get("title_sort").expect("sort");
        assert!(sort.searchable() && !sort.in_index());
        assert_eq!(s.get("edition_count").map(|f| f.kind), Some(Kind::Int));
        assert_eq!(s.unique_key, "key");
    }

    #[test]
    fn dynamic_fields_match_longest_pattern_first() {
        let s = schema();
        let names: Vec<&str> = s.dynamic.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["*_txt_de", "*_ss", "id_*", "*_s", "*"]);
        assert_eq!(
            s.get("lending_s").map(|f| f.type_name.clone()),
            Some("string".into())
        );
        let ss = s.get("tags_ss").expect("tags_ss");
        assert_eq!(
            (ss.name.as_str(), ss.type_name.as_str()),
            ("tags_ss", "strings")
        );
        assert!(ss.multi, "multiValued comes from the type");
        assert_eq!(s.get("id_goodreads").map(|f| f.multi), Some(true));
        let other = s.get("whatever").expect("catch-all");
        assert!(!other.searchable());
        assert!(glob_match("a*b*c", "aXXbYYc") && !glob_match("a*b*c", "abX"));
        assert!(glob_match("*_s", "_s") && !glob_match("x_*_s", "x_s"));
    }

    #[test]
    fn analyzers_are_built_from_the_elements() {
        let s = schema();
        let en = s.types.get("text_en_splitting").expect("type");
        assert!(en.skipped.is_empty());
        assert_eq!(en.query_analyzer.as_ref().map(|a| a.filters.len()), Some(2));
        assert_eq!(en.index.index("Wi-Fi").main, ["wi", "fi"]);
        assert_eq!(en.query.query("Wi-Fi"), [["wi-fi"]]);
        let general = s.types.get("text_general").expect("type");
        assert_eq!(
            general
                .index_analyzer
                .as_ref()
                .map(AnalyzerSpec::filter_names),
            Some(vec!["stop", "lowercase"])
        );
        assert_eq!(
            general
                .index_analyzer
                .as_ref()
                .and_then(|a| a.filters.first())
                .and_then(|f| f.attrs.get("words"))
                .map(String::as_str),
            Some("stopwords.txt")
        );
        assert_eq!(general.query.query("The X"), [["the"], ["x"]]);
        let de = s.types.get("text_de").expect("type");
        assert_eq!(de.skipped, ["germanlightstem"]);
        assert_eq!(s.analysis_warnings(), ["text_de: germanlightstem"]);
        let plain = s.types.get("string").expect("type");
        assert_eq!(plain.query.query("A b"), [["A b"]]);
    }

    #[test]
    fn enum_values_keep_config_order() {
        let s = schema();
        let f = s.get("ebook_access").expect("field");
        assert_eq!(f.kind, Kind::Enum);
        let t = s.field_type(&f).expect("type");
        assert_eq!(t.enum_values, ["no_ebook", "borrowable", "public"]);
        let without = Schema::parse(XML, None).expect("parses without enums");
        assert!(
            without
                .types
                .get("access")
                .is_some_and(|t| t.enum_values.is_empty())
        );
    }

    #[test]
    fn copy_fields_append_into_an_effective_row() {
        let s = schema();
        let doc = json!({"key": "k", "title": "Alice", "subject": ["A", "B"], "a_s": "x", "title_suggest": "old"});
        let map = doc.as_object().expect("object");
        let eff = s.effective(map);
        assert_eq!(eff.get("title_suggest"), Some(&json!(["old", "Alice"])));
        assert_eq!(eff.get("subject_facet"), Some(&json!(["A", "B"])));
        assert_eq!(eff.get("all_ss"), Some(&json!(["x"])));
        for name in ["title_suggest", "subject_facet", "all_ss", "key"] {
            let flat: Vec<&Value> = eff.get(name).map_or_else(Vec::new, |v| match v {
                Value::Array(items) => items.iter().collect(),
                other => vec![other],
            });
            assert_eq!(s.values_of(map, name), Some(flat), "{name}");
        }
        assert_eq!(s.values_of(map, "author_name"), None);
        let plain = json!({"key": "k"});
        assert!(matches!(
            s.effective(plain.as_object().expect("object")),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn undefined_type_is_an_error() {
        let bad = r#"<schema><field name="x" type="nope"/></schema>"#;
        assert!(matches!(Schema::parse(bad, None), Err(Error::Schema(_))));
        assert!(matches!(Schema::parse("<schema", None), Err(Error::Xml(_))));
    }

    #[test]
    fn parses_the_real_schema_when_present() {
        let conf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../minisolr/conf");
        if !conf.join("managed-schema.xml").is_file() {
            eprintln!("skipping: {} not found", conf.display());
            return;
        }
        let s = Schema::load(&conf).expect("real schema parses");
        assert_eq!(s.unique_key, "key");
        assert!(s.fields.len() > 100 && s.dynamic.len() > 50);
        let index = |n: &str, v: &str| {
            s.get(n)
                .and_then(|f| s.field_type(&f).map(|t| t.index.index(v).main))
                .unwrap_or_default()
        };
        assert_eq!(index("title", "Running dontstems"), ["run", "dontstems"]);
        assert_eq!(
            index("author_name", "García Márquez"),
            ["garcia", "marquez"]
        );
        assert_eq!(index("place", "New York"), ["new", "york"]);
        let query = |n: &str, v: &str| {
            s.get(n)
                .and_then(|f| s.field_type(&f).map(|t| t.query.query(v)))
                .unwrap_or_default()
        };
        assert_eq!(query("title", "aaafoo"), [["aaabar"]]);
        assert!(
            s.analysis_warnings()
                .iter()
                .any(|w| w.starts_with("text_de: "))
        );
        assert_eq!(s.get("osp_count").map(|f| f.kind), Some(Kind::Int));
        assert_eq!(s.get("public_scan_b").map(|f| f.kind), Some(Kind::Bool));
        let access = s.get("ebook_access").expect("ebook_access");
        assert_eq!(
            s.field_type(&access).map(|t| t.enum_values.clone()),
            Some(
                [
                    "no_ebook",
                    "unclassified",
                    "printdisabled",
                    "borrowable",
                    "public"
                ]
                .map(str::to_owned)
                .to_vec()
            )
        );
        assert!(
            s.copy_fields
                .contains(&("subject".to_owned(), "subject_facet".to_owned()))
        );
    }
}
