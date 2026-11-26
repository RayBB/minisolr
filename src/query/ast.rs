//! The parsed form of a Lucene / edismax query string, before it meets the schema.
//!
//! The tree keeps each clause's operator and modifier as written; which clauses become
//! `Must`, `Should` or `MustNot` depends on the default operator and on which clauses
//! turn out empty, so the compiler decides that. Nested query strings (local params
//! `v=...`, `_query_:"..."`) stay strings here and are parsed when compiled, because their
//! text can come from `$param` dereference.

use std::borrow::Cow;
use std::fmt;

/// The connective written before a clause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conj {
    /// `AND` / `&&`.
    And,
    /// `OR` / `||`.
    Or,
}

/// The prefix written before a clause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prefix {
    /// `+`.
    Required,
    /// `-`, `!` or `NOT`.
    Prohibited,
}

/// One clause of a boolean group.
#[derive(Clone, Debug, PartialEq)]
pub struct Clause<'a> {
    /// The last `AND`/`OR` seen before the clause.
    pub conj: Option<Conj>,
    /// The last `+`/`-`/`NOT` seen before the clause.
    pub prefix: Option<Prefix>,
    /// The clause itself.
    pub node: Node<'a>,
}

/// A `^n` or `^=n` suffix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Boost {
    /// `^n`: multiply the score.
    Multiply(f32),
    /// `^=n`: replace the score with a constant.
    Constant(f32),
}

/// The ends of a `[lo TO hi]` / `{lo TO hi}` range; `*` is an open end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeBounds<'a> {
    /// Lower end as written.
    pub lo: &'a str,
    /// Upper end as written.
    pub hi: &'a str,
    /// `[` rather than `{`.
    pub lo_inclusive: bool,
    /// `]` rather than `}`.
    pub hi_inclusive: bool,
}

impl RangeBounds<'_> {
    /// Whether the lower end is open (`*`).
    #[must_use]
    pub fn lo_open(&self) -> bool {
        self.lo == "*"
    }

    /// Whether the upper end is open (`*`).
    #[must_use]
    pub fn hi_open(&self) -> bool {
        self.hi == "*"
    }
}

/// Local params `{!type key=value ...}`.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalParams<'a> {
    /// The parser type (`edismax`, `lucene`, `terms`, ...); `lucene` for `{!tag=x}`.
    pub kind: &'a str,
    /// Key/value pairs in order; quoted values are unescaped. When `v` is absent and text
    /// follows the closing `}`, that text is the `v` pair.
    pub params: Vec<(&'a str, Cow<'a, str>)>,
}

impl LocalParams<'_> {
    /// The last value given for `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_ref())
    }
}

/// A query node.
#[derive(Clone, Debug, PartialEq)]
pub enum Node<'a> {
    /// A parenthesized group (or a whole query string).
    Group(Vec<Clause<'a>>),
    /// `field:( ... )`: bare terms inside go to `field`.
    FieldGroup {
        /// The field.
        field: Cow<'a, str>,
        /// The group's clauses.
        clauses: Vec<Clause<'a>>,
    },
    /// A single term, wildcard or fuzzy term; `field: None` searches the default fields.
    Term {
        /// Explicit field.
        field: Option<Cow<'a, str>>,
        /// The text, unescaped unless it holds a `*` wildcard.
        text: Cow<'a, str>,
        /// Edit distance of a `~` fuzzy term (`~` alone means 2).
        fuzzy: Option<u32>,
    },
    /// A quoted phrase.
    Phrase {
        /// Explicit field.
        field: Option<Cow<'a, str>>,
        /// The text between the quotes, unescaped.
        text: Cow<'a, str>,
        /// `~n` after the closing quote.
        slop: Option<u32>,
    },
    /// `field:[lo TO hi]`; `bounds: None` when the brackets do not hold `lo TO hi`.
    Range {
        /// The field, unescaped.
        field: Cow<'a, str>,
        /// The parsed ends.
        bounds: Option<RangeBounds<'a>>,
    },
    /// `*:*` or a bare `*`.
    All,
    /// `{!...}` local params.
    Local(LocalParams<'a>),
    /// `_query_:"..."`: a nested query string parsed with the same settings.
    Nested(Cow<'a, str>),
    /// A node with a boost suffix.
    Boosted {
        /// The boosted node.
        node: Box<Node<'a>>,
        /// The boost.
        boost: Boost,
    },
}

/// Why a query string could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// `{!` without a closing `}`.
    UnterminatedLocalParams,
    /// Something other than `key=value` inside local params, at this byte offset.
    BadLocalParam(usize),
    /// A boost whose number does not parse.
    BadBoost(String),
    /// Local params and `$param` references nest too deeply (probably a cycle).
    TooDeep,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedLocalParams => f.write_str("unterminated local params"),
            Self::BadLocalParam(at) => write!(f, "bad local param at byte {at}"),
            Self::BadBoost(b) => write!(f, "bad boost {b:?}"),
            Self::TooDeep => f.write_str("query nesting too deep"),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for crate::Error {
    fn from(e: ParseError) -> Self {
        Self::Unsupported(format!("query syntax: {e}"))
    }
}
