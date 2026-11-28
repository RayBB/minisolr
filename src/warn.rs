//! Requests that use something minisolr does not support.
//!
//! Code deep in the query path calls [`note`] with a short stable kind (`facet.range`,
//! `function:foo`, ...). The HTTP layer collects the kinds a request noted with
//! [`take`], counts them in a [`Registry`], logs them, adds them to the response and, in
//! strict mode, turns the request into an HTTP 400. Requests run start to finish on one
//! worker thread, so the pending list is thread-local.

use std::cell::RefCell;
use std::collections::BTreeMap;

/// Longest kind kept; longer ones are cut so a header and the registry stay small.
const MAX_KIND: usize = 64;
/// Longest request example kept per kind.
const MAX_EXAMPLE: usize = 300;
/// Distinct kinds tracked; further ones count under [`OVERFLOW`].
const MAX_KINDS: usize = 100;
/// Kind used once the registry holds [`MAX_KINDS`] kinds.
const OVERFLOW: &str = "other";

thread_local! {
    static PENDING: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// Records that the current request used an unsupported feature: `kind` is stable and
/// short, `detail` says what exactly. A kind noted twice in one request is kept once.
pub fn note(kind: &str, detail: &str) {
    let kind: String = kind
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != ',' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_KIND)
        .collect();
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        if !p.iter().any(|(k, _)| *k == kind) {
            p.push((kind, detail.to_owned()));
        }
    });
}

/// Removes and returns the `(kind, detail)` pairs the current thread has noted.
#[must_use]
pub fn take() -> Vec<(String, String)> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// The kind for an [`Error::Unsupported`](crate::Error::Unsupported) message.
#[must_use]
pub fn kind_of(msg: &str) -> String {
    let local_type = |m: &str| -> Option<String> {
        let rest = m.strip_prefix("{!")?;
        Some(rest.split(['}', ' ']).next()?.to_owned())
    };
    if let Some(p) = msg.strip_prefix("parameter ") {
        format!("param:{p}")
    } else if let Some(t) = msg.strip_prefix("local params type {!") {
        format!("localparam:{}", t.trim_end_matches('}'))
    } else if let Some(t) = local_type(msg) {
        format!("localparam:{t}")
    } else if msg.starts_with("undefined field") {
        "undefined-field".into()
    } else if msg.starts_with("field boost") {
        "field-boost".into()
    } else if msg.starts_with("bare term") {
        "bare-term".into()
    } else if msg.starts_with("range on") {
        "range-on-text".into()
    } else if msg.starts_with("wildcard") {
        "wildcard".into()
    } else if msg.starts_with("query syntax") {
        "query-syntax".into()
    } else if msg.starts_with("field ") || msg.starts_with("query on") {
        "field-not-searchable".into()
    } else {
        "other".into()
    }
}

/// One unsupported feature seen since the server started.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The kind.
    pub kind: String,
    /// Requests that used it.
    pub count: u64,
    /// The first such request (truncated).
    pub example: String,
}

/// Counts of unsupported features and the first request that used each.
#[derive(Debug, Default)]
pub struct Registry {
    kinds: BTreeMap<String, Entry>,
}

impl Registry {
    /// Counts one use of `kind` and keeps `example` if it is the first; returns the
    /// kind actually counted and its new count.
    pub fn record(&mut self, kind: &str, example: &str) -> (String, u64) {
        let key = if self.kinds.contains_key(kind) || self.kinds.len() < MAX_KINDS {
            kind
        } else {
            OVERFLOW
        };
        let entry = self.kinds.entry(key.to_owned()).or_insert_with(|| Entry {
            kind: key.to_owned(),
            count: 0,
            example: example.chars().take(MAX_EXAMPLE).collect(),
        });
        entry.count += 1;
        (key.to_owned(), entry.count)
    }

    /// Every kind seen, sorted by name.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        self.kinds.values().cloned().collect()
    }
}

/// Whether the `count`-th use of a kind deserves a log line: the first, then every power
/// of ten.
#[must_use]
pub fn should_log(count: u64) -> bool {
    count == 1 || (count >= 10 && count.ilog10() < 19 && 10u64.pow(count.ilog10()) == count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_dedups_sanitizes_and_take_clears() {
        let _ = take();
        note("function:foo", "a");
        note("function:foo", "b");
        note("bad kind,\u{e9}", "c");
        let got = take();
        assert_eq!(
            got,
            [
                ("function:foo".to_owned(), "a".to_owned()),
                ("bad_kind__".to_owned(), "c".to_owned())
            ]
        );
        assert!(take().is_empty());
    }

    #[test]
    fn kinds_from_messages() {
        assert_eq!(kind_of("parameter group"), "param:group");
        assert_eq!(kind_of("local params type {!bar}"), "localparam:bar");
        assert_eq!(kind_of("{!terms} value \"x\" on f"), "localparam:terms");
        assert_eq!(
            kind_of("bare term \"x\" without a default field"),
            "bare-term"
        );
        assert_eq!(kind_of("range on Text field title"), "range-on-text");
    }

    #[test]
    fn registry_counts_and_keeps_first_example() {
        let mut r = Registry::default();
        assert_eq!(r.record("a", "first"), ("a".into(), 1));
        assert_eq!(r.record("a", "second"), ("a".into(), 2));
        let e = r.entries();
        assert_eq!((e[0].count, e[0].example.as_str()), (2, "first"));
        for i in 0..MAX_KINDS {
            r.record(&format!("k{i}"), "x");
        }
        assert_eq!(r.record("new", "x").0, OVERFLOW);
        assert_eq!(r.record("a", "x").0, "a");
    }

    #[test]
    fn logs_first_and_powers_of_ten() {
        let on: Vec<u64> = (1..=1000).filter(|n| should_log(*n)).collect();
        assert_eq!(on, [1, 10, 100, 1000]);
    }
}
