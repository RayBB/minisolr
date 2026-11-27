//! Hand-written equivalents of the few Python regular expressions and conversions that
//! highlighting and spellcheck use, with Python's Unicode `\w` and `\s`.

use crate::unicode;

/// Python's `re.sub(r"\{![^}]*\}|\$\w+", " ", text)`, and with `punctuation` also
/// `|[^\w\s]`: local params, `$param` references (and every other character that is
/// neither a word character nor whitespace) become a space.
#[must_use]
pub fn scrub(text: &str, punctuation: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let after = rest.get(c.len_utf8()..).unwrap_or_default();
        if c == '{'
            && after.starts_with('!')
            && let Some(close) = after.find('}')
        {
            out.push(' ');
            rest = after.get(close + 1..).unwrap_or_default();
            continue;
        }
        if c == '$' {
            let word = after
                .char_indices()
                .find(|(_, d)| !unicode::is_word(*d))
                .map_or(after.len(), |(i, _)| i);
            if word > 0 {
                out.push(' ');
                rest = after.get(word..).unwrap_or_default();
                continue;
            }
        }
        if punctuation && !unicode::is_word(c) && !unicode::is_space(c) {
            out.push(' ');
        } else {
            out.push(c);
        }
        rest = after;
    }
    out
}

/// The matches of `\w+(?:['’]\w+)*` in `text` as byte ranges, left to right.
#[must_use]
pub fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if !unicode::is_word(c) {
            continue;
        }
        let mut end = start + c.len_utf8();
        loop {
            match chars.peek().copied() {
                Some((i, d)) if unicode::is_word(d) => {
                    end = i + d.len_utf8();
                    chars.next();
                }
                Some((_, '\'' | '\u{2019}')) => {
                    let mut ahead = chars.clone();
                    ahead.next();
                    match ahead.peek().copied() {
                        Some((k, e)) if unicode::is_word(e) => {
                            end = k + e.len_utf8();
                            chars.next();
                            chars.next();
                        }
                        _ => break,
                    }
                }
                _ => break,
            }
        }
        spans.push((start, end));
    }
    spans
}

/// Python's `int()` of a string: surrounding whitespace, an optional sign, digits with
/// single underscores between them.
#[must_use]
pub fn py_int(s: &str) -> Option<i64> {
    let t = s.trim_matches(unicode::is_space);
    let (negative, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let n: i64 = digits.replace('_', "").parse().ok()?;
    Some(if negative { -n } else { n })
}

/// Python's `items[:n]`: a negative `n` drops that many from the end.
#[must_use]
pub fn py_head<T>(mut items: Vec<T>, n: i64) -> Vec<T> {
    let len = items.len();
    let keep = if n >= 0 {
        usize::try_from(n).unwrap_or(usize::MAX).min(len)
    } else {
        len.saturating_sub(usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX))
    };
    items.truncate(keep);
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_matches_python_re_sub() {
        assert_eq!(scrub("{!edismax qf=x}sawyer $uw", false), " sawyer  ");
        assert_eq!(scrub("{!open $ a", false), "{!open $ a");
        assert_eq!(
            scrub("title:tom-sawyer's {!x} $q!", true),
            "title tom sawyer s     "
        );
        assert_eq!(scrub("{!open", true), "  open");
    }

    #[test]
    fn word_spans_join_apostrophes() {
        let text = "Tom's  o'' x’y _a-b";
        let words: Vec<&str> = word_spans(text)
            .into_iter()
            .filter_map(|(s, e)| text.get(s..e))
            .collect();
        assert_eq!(words, ["Tom's", "o", "x’y", "_a", "b"]);
        let telugu = "\u{0C30}\u{0C41}x";
        assert_eq!(word_spans(telugu).len(), 2, "marks are not \\w in Python");
    }

    #[test]
    fn python_int_and_slices() {
        assert_eq!(py_int(" 10 "), Some(10));
        assert_eq!(py_int("+3"), Some(3));
        assert_eq!(py_int("-1"), Some(-1));
        assert_eq!(py_int("1_000"), Some(1000));
        assert_eq!(py_int("1__0"), None);
        assert_eq!(py_int("x"), None);
        assert_eq!(py_head(vec![1, 2, 3], 2), [1, 2]);
        assert_eq!(py_head(vec![1, 2, 3], -1), [1, 2]);
        assert_eq!(py_head(vec![1, 2, 3], -5), Vec::<i32>::new());
        assert_eq!(py_head(vec![1, 2, 3], 10), [1, 2, 3]);
    }
}
