//! Builds the [`ast`](super::ast) for one query string, following Python minisolr's
//! `Parser` step by step.
//!
//! Two parsing decisions need outside knowledge, so the caller passes it in: whether bare
//! terms have default fields (edismax `qf`, `df`, or inside `field:(...)`), and whether a
//! name is a schema field. With default fields, `foo:bar` on an unknown `foo` is plain
//! text (edismax behaviour) and does not take the next token as its value.
//!
//! Like the Python parser, it never fails on odd input: text it cannot tokenize ends the
//! clause list, an unmatched `)` ends the query, and clauses that cannot stand alone
//! (a stray boost or bare range) are dropped. Only malformed local params and boosts are
//! errors.

use std::borrow::Cow;

use super::ast::{Boost, Clause, Conj, Node, ParseError, Prefix, RangeBounds};
use super::lexer::{self, Lexed, Modifier, Op, Token};
use super::localparams;

/// Parses `s` into the clauses of its top-level group.
///
/// `has_defaults` says whether bare terms have default fields; `known_field` whether a
/// name is defined in the schema.
///
/// # Errors
/// [`ParseError`] for malformed local params or boost numbers.
pub fn parse<'a>(
    s: &'a str,
    has_defaults: bool,
    known_field: &dyn Fn(&str) -> bool,
) -> Result<Vec<Clause<'a>>, ParseError> {
    Parser {
        s,
        pos: 0,
        known_field,
    }
    .expr(has_defaults)
}

struct Parser<'a, 'k> {
    s: &'a str,
    pos: usize,
    known_field: &'k dyn Fn(&str) -> bool,
}

impl<'a> Parser<'a, '_> {
    fn peek(&self) -> Option<Lexed<'a>> {
        lexer::next(self.s, self.pos)
    }

    /// Clauses up to the end, an unmatched `)` or text that does not tokenize.
    fn expr(&mut self, has_defaults: bool) -> Result<Vec<Clause<'a>>, ParseError> {
        let mut clauses = Vec::new();
        let mut conj = None;
        let mut prefix = None;
        while let Some(lexed) = self.peek() {
            match lexed.token {
                Token::RParen => break,
                Token::Op(op) => {
                    self.pos = lexed.end;
                    match op {
                        Op::And => conj = Some(Conj::And),
                        Op::Or => conj = Some(Conj::Or),
                        Op::Not => prefix = Some(Prefix::Prohibited),
                    }
                }
                Token::Modifier(m) => {
                    self.pos = lexed.end;
                    prefix = Some(match m {
                        Modifier::Required => Prefix::Required,
                        Modifier::Prohibited => Prefix::Prohibited,
                    });
                }
                _ => {
                    let node = self.clause(lexed, has_defaults)?;
                    let (c, p) = (conj.take(), prefix.take());
                    if let Some(node) = node {
                        clauses.push(Clause {
                            conj: c,
                            prefix: p,
                            node,
                        });
                    }
                }
            }
        }
        Ok(clauses)
    }

    fn clause(
        &mut self,
        lexed: Lexed<'a>,
        has_defaults: bool,
    ) -> Result<Option<Node<'a>>, ParseError> {
        self.pos = lexed.end;
        let node = match lexed.token {
            Token::LParen => Some(Node::Group(self.group(has_defaults)?)),
            Token::LocalParams => Some(self.local()?),
            Token::FieldRange { field, range } => Some(Node::Range {
                field: unescape(field),
                bounds: range_bounds(range),
            }),
            Token::Phrase { text, slop } => Some(phrase(None, text, slop)),
            Token::Word(w) => self.word(w, has_defaults)?,
            _ => return Ok(None),
        };
        self.boost(node)
    }

    /// The clauses after a `(` up to and including the matching `)`.
    fn group(&mut self, has_defaults: bool) -> Result<Vec<Clause<'a>>, ParseError> {
        let clauses = self.expr(has_defaults)?;
        if let Some(lexed) = self.peek().filter(|l| l.token == Token::RParen) {
            self.pos = lexed.end;
        }
        Ok(clauses)
    }

    fn boost(&mut self, node: Option<Node<'a>>) -> Result<Option<Node<'a>>, ParseError> {
        let Some(Lexed {
            token: Token::Boost { constant, value },
            end,
            ..
        }) = self.peek()
        else {
            return Ok(node);
        };
        self.pos = end;
        let Some(node) = node else {
            return Ok(None);
        };
        let n: f32 = value
            .parse()
            .map_err(|_| ParseError::BadBoost(value.to_owned()))?;
        Ok(Some(Node::Boosted {
            node: Box::new(node),
            boost: if constant {
                Boost::Constant(n)
            } else {
                Boost::Multiply(n)
            },
        }))
    }

    fn local(&mut self) -> Result<Node<'a>, ParseError> {
        let (mut local, end) = localparams::parse(self.s, self.pos)?;
        self.pos = end;
        if local.get("v").is_none() {
            let rest = self.s.get(self.pos..).unwrap_or_default().trim();
            self.pos = self.s.len();
            if !rest.is_empty() {
                local.params.push(("v", Cow::Borrowed(rest)));
            }
        }
        Ok(Node::Local(local))
    }

    fn word(&mut self, w: &'a str, has_defaults: bool) -> Result<Option<Node<'a>>, ParseError> {
        match unescaped_colon(w) {
            Some(0) => Ok(Some(bare_word(w.get(1..).unwrap_or_default().into()))),
            Some(idx) => {
                let field = unescape(w.get(..idx).unwrap_or_default());
                let rest = w.get(idx + 1..).unwrap_or_default();
                if field == "*" && rest == "*" {
                    return Ok(Some(Node::All));
                }
                if field == "_query_" {
                    return Ok(self.take_value(rest).map(|v| Node::Nested(nested_text(v))));
                }
                if (self.known_field)(&field) || !has_defaults {
                    return self.field_value(field, rest);
                }
                Ok(Some(bare_word(unescape(w))))
            }
            None => Ok(Some(bare_word(w.into()))),
        }
    }

    /// The value of `field:`: the rest of the word, or else the next token whatever it is.
    fn take_value(&mut self, rest: &'a str) -> Option<&'a str> {
        if !rest.is_empty() {
            return Some(rest);
        }
        let lexed = self.peek()?;
        self.pos = lexed.end;
        Some(lexed.text(self.s))
    }

    fn field_value(
        &mut self,
        field: Cow<'a, str>,
        mut rest: &'a str,
    ) -> Result<Option<Node<'a>>, ParseError> {
        if rest.is_empty() {
            let Some(lexed) = self.peek() else {
                return Ok(None);
            };
            match lexed.token {
                Token::LParen => {
                    self.pos = lexed.end;
                    let clauses = self.group(true)?;
                    return Ok(Some(Node::FieldGroup { field, clauses }));
                }
                Token::Phrase { text, slop } => {
                    self.pos = lexed.end;
                    return Ok(Some(phrase(Some(field), text, slop)));
                }
                Token::Range(range) => {
                    self.pos = lexed.end;
                    return Ok(Some(Node::Range {
                        field,
                        bounds: range_bounds(range),
                    }));
                }
                Token::Word(w) => {
                    self.pos = lexed.end;
                    rest = w;
                }
                _ => return Ok(None),
            }
        }
        let (rest, fuzzy) = split_fuzzy(rest);
        if rest.starts_with('"') {
            return Ok(Some(Node::Phrase {
                field: Some(field),
                text: rest.trim_matches('"').into(),
                slop: Some(0),
            }));
        }
        Ok(Some(Node::Term {
            field: Some(field),
            text: unescape_unless_wildcard(rest),
            fuzzy,
        }))
    }
}

/// A word without a field: `*` and `*:*` match everything, other text goes to the default
/// fields.
fn bare_word(w: Cow<'_, str>) -> Node<'_> {
    let (text, fuzzy) = match w {
        Cow::Borrowed(b) => {
            let (t, f) = split_fuzzy(b);
            (unescape_unless_wildcard(t), f)
        }
        Cow::Owned(o) => {
            let (t, f) = split_fuzzy(&o);
            (Cow::Owned(unescape_unless_wildcard(t).into_owned()), f)
        }
    };
    if text == "*:*" || text == "*" {
        return Node::All;
    }
    Node::Term {
        field: None,
        text,
        fuzzy,
    }
}

fn phrase<'a>(field: Option<Cow<'a, str>>, raw: &'a str, slop: Option<&str>) -> Node<'a> {
    let text = if raw.contains('\\') {
        Cow::Owned(raw.replace("\\\"", "\"").replace("\\\\", "\\"))
    } else {
        Cow::Borrowed(raw)
    };
    Node::Phrase {
        field,
        text,
        slop: slop.map(saturating_u32),
    }
}

/// The text of `_query_:"..."`: quotes stripped and `\"`, `\\` resolved.
fn nested_text(v: &str) -> Cow<'_, str> {
    if !v.starts_with('"') {
        return Cow::Borrowed(v);
    }
    let last = v.char_indices().next_back().map_or(0, |(i, _)| i);
    let inner = v.get(1..last.max(1)).unwrap_or_default();
    if inner.contains('\\') {
        Cow::Owned(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
    } else {
        Cow::Borrowed(inner)
    }
}

/// Byte offset of the first `:` not preceded by a backslash.
fn unescaped_colon(w: &str) -> Option<usize> {
    let mut prev = None;
    for (i, c) in w.char_indices() {
        if c == ':' && prev != Some('\\') {
            return Some(i);
        }
        prev = Some(c);
    }
    None
}

/// `term~2` gives `("term", Some(2))`; a bare `~` means distance 2.
#[must_use]
pub fn split_fuzzy(w: &str) -> (&str, Option<u32>) {
    match w.rsplit_once('~') {
        Some((head, digits)) if digits.bytes().all(|b| b.is_ascii_digit()) => (
            head,
            Some(if digits.is_empty() {
                2
            } else {
                saturating_u32(digits)
            }),
        ),
        _ => (w, None),
    }
}

fn saturating_u32(digits: &str) -> u32 {
    digits.bytes().fold(0u32, |n, b| {
        n.saturating_mul(10).saturating_add(u32::from(b - b'0'))
    })
}

/// Drops the backslash of each `\x` escape.
#[must_use]
pub fn unescape(t: &str) -> Cow<'_, str> {
    if !t.contains('\\') {
        return Cow::Borrowed(t);
    }
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(e) => out.push(e),
                None => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn unescape_unless_wildcard(t: &str) -> Cow<'_, str> {
    if t.contains('*') {
        Cow::Borrowed(t)
    } else {
        unescape(t)
    }
}

/// Parses `[lo TO hi]` like Python's `([\[{])\s*(\S+)\s+TO\s+(\S+)\s*([\]}])`.
#[must_use]
pub fn range_bounds(raw: &str) -> Option<RangeBounds<'_>> {
    let mut chars = raw.chars();
    let lo_inclusive = match chars.next()? {
        '[' => true,
        '{' => false,
        _ => return None,
    };
    let hi_inclusive = match chars.next_back()? {
        ']' => true,
        '}' => false,
        _ => return None,
    };
    let inner = chars.as_str().trim_start();
    let lo_end = inner.find(char::is_whitespace)?;
    let lo = inner.get(..lo_end)?;
    let to = inner.get(lo_end..)?.trim_start();
    let hi_part = to.strip_prefix("TO")?;
    if !hi_part.starts_with(char::is_whitespace) {
        return None;
    }
    let hi = hi_part.trim();
    if hi.is_empty() || hi.contains(char::is_whitespace) {
        return None;
    }
    Some(RangeBounds {
        lo,
        hi,
        lo_inclusive,
        hi_inclusive,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::LocalParams;

    fn known(f: &str) -> bool {
        matches!(
            f,
            "title" | "subject" | "type" | "edition_count" | "ia" | "language"
        )
    }

    fn lucene(s: &str) -> Vec<Clause<'_>> {
        parse(s, false, &known).expect("parses")
    }

    fn edismax(s: &str) -> Vec<Clause<'_>> {
        parse(s, true, &known).expect("parses")
    }

    fn term<'a>(field: Option<&'a str>, text: &'a str) -> Node<'a> {
        Node::Term {
            field: field.map(Cow::Borrowed),
            text: Cow::Borrowed(text),
            fuzzy: None,
        }
    }

    fn clause(conj: Option<Conj>, prefix: Option<Prefix>, node: Node<'_>) -> Clause<'_> {
        Clause { conj, prefix, node }
    }

    #[test]
    fn field_terms_operators_and_prefixes() {
        assert_eq!(
            lucene("title:alice AND -type:work OR NOT x:y +z:w"),
            [
                clause(None, None, term(Some("title"), "alice")),
                clause(
                    Some(Conj::And),
                    Some(Prefix::Prohibited),
                    term(Some("type"), "work")
                ),
                clause(
                    Some(Conj::Or),
                    Some(Prefix::Prohibited),
                    term(Some("x"), "y")
                ),
                clause(None, Some(Prefix::Required), term(Some("z"), "w")),
            ]
        );
    }

    #[test]
    fn bare_words_and_match_all() {
        assert_eq!(
            edismax("lewis * *:* :x"),
            [
                clause(None, None, term(None, "lewis")),
                clause(None, None, Node::All),
                clause(None, None, Node::All),
                clause(None, None, term(None, "x")),
            ]
        );
    }

    #[test]
    fn phrases_slop_and_escapes() {
        assert_eq!(
            lucene(r#"title:"tom \"x\" sawyer"~3 "a b""#),
            [
                clause(
                    None,
                    None,
                    Node::Phrase {
                        field: Some("title".into()),
                        text: Cow::Owned("tom \"x\" sawyer".into()),
                        slop: Some(3)
                    }
                ),
                clause(
                    None,
                    None,
                    Node::Phrase {
                        field: None,
                        text: "a b".into(),
                        slop: None
                    }
                ),
            ]
        );
    }

    #[test]
    fn nested_groups_and_field_groups() {
        let got = lucene("(a:b (c:d)) title:(x OR y)");
        assert_eq!(
            got,
            [
                clause(
                    None,
                    None,
                    Node::Group(vec![
                        clause(None, None, term(Some("a"), "b")),
                        clause(
                            None,
                            None,
                            Node::Group(vec![clause(None, None, term(Some("c"), "d"))])
                        ),
                    ])
                ),
                clause(
                    None,
                    None,
                    Node::FieldGroup {
                        field: "title".into(),
                        clauses: vec![
                            clause(None, None, term(None, "x")),
                            clause(Some(Conj::Or), None, term(None, "y")),
                        ]
                    }
                ),
            ]
        );
        assert_eq!(lucene("a:b) c:d").len(), 1, "unmatched ) ends the query");
    }

    #[test]
    fn ranges_inclusive_exclusive_and_open() {
        let bounds = |lo, hi, li, hi_inc| {
            Some(RangeBounds {
                lo,
                hi,
                lo_inclusive: li,
                hi_inclusive: hi_inc,
            })
        };
        assert_eq!(
            lucene("edition_count:[5 TO *] x:{a TO b] y: [ 1  TO 2 ] z:[1 TO 2 3]"),
            [
                clause(
                    None,
                    None,
                    Node::Range {
                        field: "edition_count".into(),
                        bounds: bounds("5", "*", true, true)
                    }
                ),
                clause(
                    None,
                    None,
                    Node::Range {
                        field: "x".into(),
                        bounds: bounds("a", "b", false, true)
                    }
                ),
                clause(
                    None,
                    None,
                    Node::Range {
                        field: "y".into(),
                        bounds: bounds("1", "2", true, true)
                    }
                ),
                clause(
                    None,
                    None,
                    Node::Range {
                        field: "z".into(),
                        bounds: None
                    }
                ),
            ]
        );
        assert_eq!(lucene("[1 TO 2]"), [], "a bare range is dropped");
    }

    #[test]
    fn wildcards_prefix_fuzzy_and_exists() {
        assert_eq!(
            lucene(r"title:fl* ia:* title:twian~ title:x~1 title:a\-b"),
            [
                clause(None, None, term(Some("title"), "fl*")),
                clause(None, None, term(Some("ia"), "*")),
                clause(
                    None,
                    None,
                    Node::Term {
                        field: Some("title".into()),
                        text: "twian".into(),
                        fuzzy: Some(2)
                    }
                ),
                clause(
                    None,
                    None,
                    Node::Term {
                        field: Some("title".into()),
                        text: "x".into(),
                        fuzzy: Some(1)
                    }
                ),
                clause(None, None, term(Some("title"), "a-b")),
            ]
        );
    }

    #[test]
    fn boosts_and_constant_scores() {
        let got = lucene("language:eng^40 (x:y)^=1000 ^3");
        assert_eq!(
            got,
            [
                clause(
                    None,
                    None,
                    Node::Boosted {
                        node: Box::new(term(Some("language"), "eng")),
                        boost: Boost::Multiply(40.0)
                    }
                ),
                clause(
                    None,
                    None,
                    Node::Boosted {
                        node: Box::new(Node::Group(vec![clause(None, None, term(Some("x"), "y"))])),
                        boost: Boost::Constant(1000.0)
                    }
                ),
            ]
        );
        assert_eq!(
            parse("a:b^1.2.3", false, &known),
            Err(ParseError::BadBoost("1.2.3".into()))
        );
    }

    #[test]
    fn edismax_unknown_field_is_text_and_does_not_take_the_next_token() {
        assert_eq!(
            edismax("foo:bar title: alice"),
            [
                clause(None, None, term(None, "foo:bar")),
                clause(None, None, term(Some("title"), "alice")),
            ]
        );
        assert_eq!(
            edismax("foo: bar"),
            [
                clause(None, None, term(None, "foo:")),
                clause(None, None, term(None, "bar")),
            ]
        );
        assert_eq!(
            lucene("foo: bar"),
            [clause(None, None, term(Some("foo"), "bar"))]
        );
    }

    #[test]
    fn local_params_and_nested_queries() {
        let got = lucene(
            r#"+({!edismax q.op="AND" qf="a^2 b" v=$uq}) +(_query_:"{!parent which=type:work v=$f}" OR edition_count:0)"#,
        );
        let Some(Clause {
            prefix: Some(Prefix::Required),
            node: Node::Group(inner),
            ..
        }) = got.first()
        else {
            panic!("first clause: {got:?}");
        };
        assert_eq!(
            inner.first().map(|c| &c.node),
            Some(&Node::Local(LocalParams {
                kind: "edismax",
                params: vec![
                    ("q.op", "AND".into()),
                    ("qf", "a^2 b".into()),
                    ("v", "$uq".into())
                ]
            }))
        );
        let Some(Node::Group(second)) = got.get(1).map(|c| &c.node) else {
            panic!("second clause: {got:?}");
        };
        assert_eq!(
            second.first().map(|c| &c.node),
            Some(&Node::Nested("{!parent which=type:work v=$f}".into()))
        );
        assert_eq!(second.get(1).map(|c| c.conj), Some(Some(Conj::Or)));
    }

    #[test]
    fn local_params_without_v_take_the_rest_of_the_string() {
        assert_eq!(
            lucene("{!tag=lang}language:eng x"),
            [clause(
                None,
                None,
                Node::Local(LocalParams {
                    kind: "lucene",
                    params: vec![("tag", "lang".into()), ("v", "language:eng x".into())]
                })
            )]
        );
        assert_eq!(
            parse("{!terms f=x", false, &known),
            Err(ParseError::UnterminatedLocalParams)
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(split_fuzzy("a~b~"), ("a~b", Some(2)));
        assert_eq!(split_fuzzy("a~b"), ("a~b", None));
        assert_eq!(unescape(r"a\:b\\c\"), r"a:b\c\");
        assert_eq!(nested_text("\"x\\\"y\""), "x\"y");
        assert_eq!(nested_text("plain"), "plain");
        assert_eq!(range_bounds("[aTO TO b]").map(|b| b.lo), Some("aTO"));
        assert_eq!(range_bounds("[a TOb]"), None);
    }
}
