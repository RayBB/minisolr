//! The Unicode character properties the Python analyzers use, from tables generated out of
//! Python's `unicodedata` (`tools/gen_unicode.py`), so both see the same Unicode version and
//! the same categories.
//!
//! Rust's `char` predicates follow derived properties instead (`is_alphabetic` includes
//! `Other_Alphabetic` symbols such as circled letters, `is_numeric` includes `No`, and
//! `unicode-normalization` cannot tell spacing marks (`Mc`) from nonspacing marks (`Mn`)),
//! which is why the analyzers ask these tables.

// Generated data: long hex literals are one per run, separators would only add noise.
#[allow(clippy::unreadable_literal)]
mod table;

pub use table::VERSION;

/// The general category of a code point, reduced to what the analyzers distinguish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Anything not listed below (punctuation, symbols, separators, controls, unassigned).
    Other,
    /// `Lu` or `Lt`.
    Upper,
    /// `Ll`.
    Lower,
    /// `Lm` or `Lo`.
    OtherLetter,
    /// `Mn`, a nonspacing mark.
    Nonspacing,
    /// `Mc`, a spacing combining mark.
    Spacing,
    /// `Me`, an enclosing mark.
    Enclosing,
    /// `Nd`, a decimal digit.
    Digit,
    /// `Nl` or `No`.
    OtherNumber,
}

/// The class of `c`.
#[must_use]
pub fn class(c: char) -> Class {
    let cp = u32::from(c);
    let at = table::CLASSES.partition_point(|run| run >> 4 <= cp);
    let run = at
        .checked_sub(1)
        .and_then(|i| table::CLASSES.get(i))
        .copied()
        .unwrap_or_default();
    match run & 0xf {
        1 => Class::Upper,
        2 => Class::Lower,
        3 => Class::OtherLetter,
        4 => Class::Nonspacing,
        5 => Class::Spacing,
        6 => Class::Enclosing,
        7 => Class::Digit,
        8 => Class::OtherNumber,
        _ => Class::Other,
    }
}

/// A letter (`L*`).
#[must_use]
pub fn is_letter(c: char) -> bool {
    matches!(class(c), Class::Upper | Class::Lower | Class::OtherLetter)
}

/// A mark (`M*`).
#[must_use]
pub fn is_mark(c: char) -> bool {
    matches!(
        class(c),
        Class::Nonspacing | Class::Spacing | Class::Enclosing
    )
}

/// A number (`N*`).
#[must_use]
pub fn is_number(c: char) -> bool {
    matches!(class(c), Class::Digit | Class::OtherNumber)
}

/// Python's `str.isalnum()` for one character (letters and numbers); with `_` this is
/// what `\w` matches in a Python regular expression.
#[must_use]
pub fn is_alnum(c: char) -> bool {
    is_letter(c) || is_number(c)
}

/// What `\w` matches in a Python regular expression.
#[must_use]
pub fn is_word(c: char) -> bool {
    c == '_' || is_alnum(c)
}

/// Python's `str.isspace()` for one character (what `str.split()` and `\s` split on).
#[must_use]
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// Python's `str.split()` with no argument.
pub fn split_whitespace(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|t| !t.is_empty())
}

/// Appends Python's `str.casefold()` of `c` to `out`.
pub fn push_casefold(c: char, out: &mut String) {
    match table::CASEFOLD.binary_search_by_key(&c, |(k, _)| *k) {
        Ok(i) => out.push_str(table::CASEFOLD.get(i).map_or("", |(_, f)| f)),
        Err(_) => out.extend(c.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telugu_spacing_marks_are_not_nonspacing() {
        // TELUGU VOWEL SIGN AA (Mn) and TELUGU VOWEL SIGN U (Mc).
        assert_eq!(class('\u{0C3E}'), Class::Nonspacing);
        assert_eq!(class('\u{0C41}'), Class::Spacing);
        assert_eq!(class('\u{0903}'), Class::Spacing);
        assert_eq!(class('\u{20DD}'), Class::Enclosing);
        assert_eq!(class('\u{0301}'), Class::Nonspacing);
    }

    #[test]
    fn categories_follow_python_not_rust_derived_properties() {
        assert_eq!(class('A'), Class::Upper);
        assert_eq!(class('\u{01C5}'), Class::Upper, "Lt counts as upper");
        assert_eq!(class('a'), Class::Lower);
        assert_eq!(
            class('\u{00AA}'),
            Class::OtherLetter,
            "ª is Lo, not lowercase"
        );
        assert_eq!(class('\u{24B6}'), Class::Other, "circled A is a symbol");
        assert_eq!(class('\u{00B2}'), Class::OtherNumber);
        assert_eq!(class('7'), Class::Digit);
        assert_eq!(class('-'), Class::Other);
        assert_eq!(class('\u{10FFFF}'), Class::Other);
        assert!(is_word('_') && is_word('\u{4E00}') && !is_word('\u{0301}'));
    }

    #[test]
    fn whitespace_and_casefold_match_python() {
        assert_eq!(
            split_whitespace("a\u{1c}b\u{a0} c ").collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        let fold = |s: &str| {
            let mut out = String::new();
            s.chars().for_each(|c| push_casefold(c, &mut out));
            out
        };
        assert_eq!(fold("Straße ẞ ς ſ"), "strasse ss σ s");
        assert_eq!(fold("\u{13A0}"), "\u{13A0}", "Cherokee folds to uppercase");
    }
}
