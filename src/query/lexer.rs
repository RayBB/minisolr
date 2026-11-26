//! Tokens of the Lucene classic query syntax, as Python minisolr's `TOK` regex splits them.
//!
//! The alternatives are tried in the same order as the regex, and the first that matches
//! wins: `{!`, `(`, `)`, a quoted phrase with optional `~slop`, `field:[a TO b]`, a bare
//! range, a boost `^n` / `^=n`, an operator (`AND`, `OR`, `NOT` before a space or `(`,
//! `&&`, `||`), a `+ - !` modifier directly before a non-space, and finally a word: a run
//! of anything except whitespace, `( ) " ^`, where a backslash escapes the next character.
//! Some quirks follow from that order and are kept on purpose: `-f:[1 TO 2]` lexes as one
//! range on the field `-f`, and `AND` at the very end of a string is a word.

/// A boolean operator keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `AND` or `&&`.
    And,
    /// `OR` or `||`.
    Or,
    /// `NOT`.
    Not,
}

/// A clause prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    /// `+`.
    Required,
    /// `-` or `!`.
    Prohibited,
}

/// One token. Slices borrow from the query string; escapes are left in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    /// `{!`, the start of local params.
    LocalParams,
    /// `(`.
    LParen,
    /// `)`.
    RParen,
    /// `"text"` or `"text"~slop`: the text between the quotes and the slop digits.
    Phrase {
        /// Text between the quotes, escapes not yet resolved.
        text: &'a str,
        /// The digits after `~`, if any.
        slop: Option<&'a str>,
    },
    /// `field:[lo TO hi]` in one token.
    FieldRange {
        /// The field name, escapes not yet resolved.
        field: &'a str,
        /// The bracketed range, brackets included.
        range: &'a str,
    },
    /// A bracketed range, brackets included.
    Range(&'a str),
    /// `^n` or `^=n`.
    Boost {
        /// True for `^=` (constant score).
        constant: bool,
        /// The number after `^` or `^=`.
        value: &'a str,
    },
    /// A boolean operator.
    Op(Op),
    /// A clause modifier.
    Modifier(Modifier),
    /// Any other run of characters.
    Word(&'a str),
}

/// A token with its byte span in the query string (leading whitespace excluded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lexed<'a> {
    /// The token.
    pub token: Token<'a>,
    /// Byte offset of the first character of the token.
    pub start: usize,
    /// Byte offset just after the token.
    pub end: usize,
}

impl<'a> Lexed<'a> {
    /// The token's source text.
    #[must_use]
    pub fn text(&self, s: &'a str) -> &'a str {
        s.get(self.start..self.end).unwrap_or_default()
    }
}

/// The token starting at byte `pos` of `s` after optional whitespace, or `None` at the end
/// or where nothing matches (an unterminated quote, a lone `^`).
#[must_use]
pub fn next(s: &str, pos: usize) -> Option<Lexed<'_>> {
    let rest = s.get(pos..)?;
    let start = pos + (rest.len() - rest.trim_start().len());
    let text = s.get(start..)?;
    let (token, len) = lex_at(text)?;
    Some(Lexed {
        token,
        start,
        end: start + len,
    })
}

/// Splits a whole string into tokens, stopping where nothing matches.
pub fn tokens(s: &str) -> impl Iterator<Item = Token<'_>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let lexed = next(s, pos)?;
        pos = lexed.end;
        Some(lexed.token)
    })
}

fn lex_at(t: &str) -> Option<(Token<'_>, usize)> {
    if t.starts_with("{!") {
        return Some((Token::LocalParams, 2));
    }
    match t.chars().next()? {
        '(' => return Some((Token::LParen, 1)),
        ')' => return Some((Token::RParen, 1)),
        _ => {}
    }
    phrase(t)
        .or_else(|| field_range(t))
        .or_else(|| range(t).map(|n| (Token::Range(t.get(..n).unwrap_or_default()), n)))
        .or_else(|| boost(t))
        .or_else(|| op(t))
        .or_else(|| modifier(t))
        .or_else(|| word(t))
}

/// `"(?:\\.|[^"\\])*"(?:~\d+)?`
fn phrase(t: &str) -> Option<(Token<'_>, usize)> {
    let body = t.strip_prefix('"')?;
    let mut chars = body.char_indices();
    let close = loop {
        match chars.next()? {
            (_, '\\') => {
                if chars.next()?.1 == '\n' {
                    return None;
                }
            }
            (i, '"') => break i,
            _ => {}
        }
    };
    let text = body.get(..close)?;
    let mut end = 1 + close + 1;
    let after = t.get(end..).unwrap_or_default();
    let mut slop = None;
    if let Some(digits) = after.strip_prefix('~') {
        let n = digits.bytes().take_while(u8::is_ascii_digit).count();
        if n > 0 {
            slop = digits.get(..n);
            end += 1 + n;
        }
    }
    Some((Token::Phrase { text, slop }, end))
}

fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// `(?:[\w.\-]|\\.)+:` followed by a range.
fn field_range(t: &str) -> Option<(Token<'_>, usize)> {
    let mut chars = t.char_indices().peekable();
    let mut colon = None;
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some((_, '\n')) | None => return None,
                Some(_) => {}
            }
        } else if is_word_char(c) || c == '.' || c == '-' {
        } else if c == ':' && i > 0 {
            colon = Some(i);
            break;
        } else {
            return None;
        }
    }
    let colon = colon?;
    let after = t.get(colon + 1..)?;
    let n = range(after)?;
    Some((
        Token::FieldRange {
            field: t.get(..colon)?,
            range: after.get(..n)?,
        },
        colon + 1 + n,
    ))
}

/// `[\[{][^\]}]*\s+TO\s+[^\]}]*[\]}]`: the length of the range at the start of `t`.
fn range(t: &str) -> Option<usize> {
    if !t.starts_with(['[', '{']) {
        return None;
    }
    let close = t.find([']', '}'])?;
    let inner = t.get(1..close)?;
    let has_to = inner.match_indices("TO").any(|(i, _)| {
        let before = inner.get(..i).and_then(|b| b.chars().next_back());
        let after = inner.get(i + 2..).and_then(|a| a.chars().next());
        before.is_some_and(char::is_whitespace) && after.is_some_and(char::is_whitespace)
    });
    has_to.then_some(close + 1)
}

/// `\^=?[\d.]+`
fn boost(t: &str) -> Option<(Token<'_>, usize)> {
    let rest = t.strip_prefix('^')?;
    let (constant, rest) = match rest.strip_prefix('=') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let n = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    if n == 0 {
        return None;
    }
    let skipped = 1 + usize::from(constant);
    Some((
        Token::Boost {
            constant,
            value: rest.get(..n)?,
        },
        skipped + n,
    ))
}

/// `&&|\|\||AND(?=\s|\()|OR(?=\s|\()|NOT(?=\s|\()`
fn op(t: &str) -> Option<(Token<'_>, usize)> {
    if t.starts_with("&&") {
        return Some((Token::Op(Op::And), 2));
    }
    if t.starts_with("||") {
        return Some((Token::Op(Op::Or), 2));
    }
    for (word, op) in [("AND", Op::And), ("OR", Op::Or), ("NOT", Op::Not)] {
        if let Some(after) = t.strip_prefix(word)
            && after
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || c == '(')
        {
            return Some((Token::Op(op), word.len()));
        }
    }
    None
}

/// `[+\-!](?=[^\s])`
fn modifier(t: &str) -> Option<(Token<'_>, usize)> {
    let mut chars = t.chars();
    let m = match chars.next()? {
        '+' => Modifier::Required,
        '-' | '!' => Modifier::Prohibited,
        _ => return None,
    };
    chars
        .next()
        .is_some_and(|c| !c.is_whitespace())
        .then_some((Token::Modifier(m), 1))
}

/// `(?:\\.|[^\s()"^])+`
fn word(t: &str) -> Option<(Token<'_>, usize)> {
    let mut chars = t.char_indices().peekable();
    let mut end = 0;
    while let Some((i, c)) = chars.next() {
        if c == '\\'
            && let Some(&(j, e)) = chars.peek()
            && e != '\n'
        {
            chars.next();
            end = j + e.len_utf8();
            continue;
        }
        if c.is_whitespace() || matches!(c, '(' | ')' | '"' | '^') {
            break;
        }
        end = i + c.len_utf8();
    }
    (end > 0).then(|| (Token::Word(t.get(..end).unwrap_or_default()), end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(s: &str) -> Vec<Token<'_>> {
        tokens(s).collect()
    }

    #[test]
    fn words_parens_and_operators() {
        assert_eq!(
            all("(a OR b) AND c && d || NOT e"),
            [
                Token::LParen,
                Token::Word("a"),
                Token::Op(Op::Or),
                Token::Word("b"),
                Token::RParen,
                Token::Op(Op::And),
                Token::Word("c"),
                Token::Op(Op::And),
                Token::Word("d"),
                Token::Op(Op::Or),
                Token::Op(Op::Not),
                Token::Word("e"),
            ]
        );
    }

    #[test]
    fn operator_needs_space_or_paren_after_it() {
        assert_eq!(all("ANDROID"), [Token::Word("ANDROID")]);
        assert_eq!(all("a AND"), [Token::Word("a"), Token::Word("AND")]);
        assert_eq!(
            all("NOT(a)"),
            [
                Token::Op(Op::Not),
                Token::LParen,
                Token::Word("a"),
                Token::RParen
            ]
        );
        assert_eq!(all("and"), [Token::Word("and")]);
    }

    #[test]
    fn modifiers_only_before_non_space() {
        assert_eq!(
            all("+a -b !c + d"),
            [
                Token::Modifier(Modifier::Required),
                Token::Word("a"),
                Token::Modifier(Modifier::Prohibited),
                Token::Word("b"),
                Token::Modifier(Modifier::Prohibited),
                Token::Word("c"),
                Token::Word("+"),
                Token::Word("d"),
            ]
        );
        assert_eq!(all("foo-bar"), [Token::Word("foo-bar")]);
    }

    #[test]
    fn phrases_with_slop_and_escapes() {
        assert_eq!(
            all(r#"title:"a \"b\" c"~3 "x""#),
            [
                Token::Word("title:"),
                Token::Phrase {
                    text: r#"a \"b\" c"#,
                    slop: Some("3")
                },
                Token::Phrase {
                    text: "x",
                    slop: None
                },
            ]
        );
        assert_eq!(all("\"open"), []);
        assert_eq!(all("a \"open"), [Token::Word("a")]);
    }

    #[test]
    fn ranges_with_and_without_field() {
        assert_eq!(
            all("osp_count:[1 TO *] OR {a TO b}"),
            [
                Token::FieldRange {
                    field: "osp_count",
                    range: "[1 TO *]"
                },
                Token::Op(Op::Or),
                Token::Range("{a TO b}"),
            ]
        );
        assert_eq!(
            all("-f:[1 TO 2]"),
            [Token::FieldRange {
                field: "-f",
                range: "[1 TO 2]"
            }]
        );
        assert_eq!(all("[abc]"), [Token::Word("[abc]")]);
        assert_eq!(
            all(r"a\:b:[1 TO 2]"),
            [Token::FieldRange {
                field: r"a\:b",
                range: "[1 TO 2]"
            }]
        );
    }

    #[test]
    fn boosts_and_constant_scores() {
        assert_eq!(
            all("cover_i:*^2 (x)^=1000 a^ b"),
            [
                Token::Word("cover_i:*"),
                Token::Boost {
                    constant: false,
                    value: "2"
                },
                Token::LParen,
                Token::Word("x"),
                Token::RParen,
                Token::Boost {
                    constant: true,
                    value: "1000"
                },
                Token::Word("a"),
            ]
        );
    }

    #[test]
    fn local_params_start_and_escaped_words() {
        assert_eq!(
            all(r"{!terms f=x} a\ b\(c\) {x"),
            [
                Token::LocalParams,
                Token::Word("terms"),
                Token::Word("f=x}"),
                Token::Word(r"a\ b\(c\)"),
                Token::Word("{x"),
            ]
        );
        assert_eq!(
            all("wild*card? fuzz~1"),
            [Token::Word("wild*card?"), Token::Word("fuzz~1")]
        );
        assert_eq!(all("trailing\\"), [Token::Word("trailing\\")]);
    }

    #[test]
    fn spans_skip_leading_whitespace() {
        let s = "  _query_:\"{!x}\"";
        let first = next(s, 0).expect("word");
        assert_eq!((first.start, first.end), (2, 10));
        let second = next(s, first.end).expect("phrase");
        assert_eq!(second.text(s), "\"{!x}\"");
        assert_eq!(next(s, second.end), None);
    }
}
