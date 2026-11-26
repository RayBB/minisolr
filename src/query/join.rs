//! `{!parent which=... v=... filters=...}` block join, done in memory as Python's
//! `parent_join`: run the child query (with the filters) over all rows, take the `_root_`
//! of every matching child (a row whose `_root_` differs from its own key), and match
//! those root documents with score 0. `which` is not evaluated: a child's root is always
//! its parent, which is what Open Library's `which=type:work` selects.

use std::collections::BTreeSet;

use serde_json::Value;
use tantivy::Term;
use tantivy::collector::DocSetCollector;
use tantivy::query::{ConstScoreQuery, EmptyQuery, Query, TermSetQuery};

use super::compile::{Compiler, QueryResult};
use super::ord_of;

impl Compiler<'_> {
    /// The parents of the rows matching `child_q` and every filter in `filters`.
    pub(super) fn parent_join(&mut self, child_q: &str, filters: &[&str]) -> QueryResult {
        let mark = self.boosts.len();
        let query = self.run_query(child_q, filters);
        // Function boosts of the child query do not score the parents.
        self.boosts.truncate(mark);
        let query = query?;
        let store = self.store;
        let searcher = store.reader.searcher();
        let key_name = store.schema.unique_key.as_str();
        let mut roots: BTreeSet<&str> = BTreeSet::new();
        for addr in searcher.search(&*query, &DocSetCollector)? {
            let Some(doc) = store.docs.get(ord_of(&searcher, addr)?) else {
                continue;
            };
            let root = doc.get("_root_").and_then(Value::as_str);
            if let Some(root) = root
                && doc.get(key_name).and_then(Value::as_str) != Some(root)
            {
                roots.insert(root);
            }
        }
        let empty = || Ok(Some(Box::new(EmptyQuery) as Box<dyn Query>));
        if roots.is_empty() {
            return empty();
        }
        let Some(key) = store.fields.get(key_name) else {
            return empty();
        };
        let terms = roots
            .into_iter()
            .map(|r| Term::from_field_text(key.handle, r));
        Ok(Some(Box::new(ConstScoreQuery::new(
            Box::new(TermSetQuery::new(terms)),
            0.0,
        ))))
    }
}
