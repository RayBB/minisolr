//! The original Porter stemmer, as Snowball defines it in `porter.sbl`.
//!
//! Lucene's `PorterStemFilter` and the Python `snowballstemmer("porter")` use this
//! algorithm, not the newer Snowball English (Porter2) that `rust-stemmers` ships.

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

/// Longest matching suffix from `table`, as its length and action number.
fn longest(w: &[char], table: &[(&str, u8)]) -> Option<(usize, u8)> {
    table
        .iter()
        .filter(|(s, _)| {
            w.len() >= s.chars().count()
                && s.chars().rev().zip(w.iter().rev()).all(|(a, b)| a == *b)
        })
        .map(|(s, action)| (s.chars().count(), *action))
        .max_by_key(|(len, _)| *len)
}

fn replace_tail(w: &mut Vec<char>, len: usize, with: &str) {
    w.truncate(w.len() - len);
    w.extend(with.chars());
}

/// Region starts: the position after the first non-vowel that follows a vowel.
fn region_after(w: &[char], from: usize) -> usize {
    let vowel = (from..w.len()).find(|&i| w.get(i).copied().is_some_and(is_vowel));
    let non_vowel = vowel
        .and_then(|v| (v + 1..w.len()).find(|&i| w.get(i).copied().is_some_and(|c| !is_vowel(c))));
    non_vowel.map_or(w.len(), |i| i + 1)
}

/// A short syllable ending at `end`: non-vowel (not w, x, Y), vowel, non-vowel.
fn short_vowel_at(w: &[char], end: usize) -> bool {
    if end < 3 {
        return false;
    }
    let at = |i: usize| w.get(i).copied().unwrap_or('Y');
    !matches!(
        at(end - 1),
        'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'w' | 'x' | 'Y'
    ) && is_vowel(at(end - 2))
        && !is_vowel(at(end - 3))
}

const STEP_1A: &[(&str, u8)] = &[("s", 3), ("ies", 2), ("sses", 1), ("ss", 0)];
const STEP_1B: &[(&str, u8)] = &[("ed", 2), ("eed", 1), ("ing", 2)];
const STEP_1B_FIX: &[(&str, u8)] = &[
    ("", 3),
    ("bb", 2),
    ("dd", 2),
    ("ff", 2),
    ("gg", 2),
    ("bl", 1),
    ("mm", 2),
    ("nn", 2),
    ("pp", 2),
    ("rr", 2),
    ("at", 1),
    ("tt", 2),
    ("iz", 1),
];
const STEP_2: &[(&str, &str)] = &[
    ("anci", "ance"),
    ("enci", "ence"),
    ("abli", "able"),
    ("eli", "e"),
    ("alli", "al"),
    ("ousli", "ous"),
    ("entli", "ent"),
    ("aliti", "al"),
    ("biliti", "ble"),
    ("iviti", "ive"),
    ("tional", "tion"),
    ("ational", "ate"),
    ("alism", "al"),
    ("ation", "ate"),
    ("ization", "ize"),
    ("izer", "ize"),
    ("ator", "ate"),
    ("iveness", "ive"),
    ("fulness", "ful"),
    ("ousness", "ous"),
];
const STEP_3: &[(&str, &str)] = &[
    ("icate", "ic"),
    ("ative", ""),
    ("alize", "al"),
    ("iciti", "ic"),
    ("ical", "ic"),
    ("ful", ""),
    ("ness", ""),
];
const STEP_4: &[&str] = &[
    "ic", "ance", "ence", "able", "ible", "ate", "ive", "ize", "iti", "al", "ism", "ion", "er",
    "ous", "ant", "ent", "ment", "ement", "ou",
];

/// Longest matching suffix of `w` from a replacement table.
fn longest_pair<'t>(w: &[char], table: &'t [(&str, &str)]) -> Option<(usize, &'t str)> {
    let keyed: Vec<(&str, u8)> = table.iter().zip(1u8..).map(|((s, _), i)| (*s, i)).collect();
    let (len, idx) = longest(w, &keyed)?;
    table.get(usize::from(idx) - 1).map(|(_, r)| (len, *r))
}

/// Stems one lowercase word.
#[must_use]
pub fn stem(word: &str) -> String {
    let mut w: Vec<char> = word.chars().collect();
    for i in 0..w.len() {
        let prev_vowel = i
            .checked_sub(1)
            .and_then(|p| w.get(p))
            .copied()
            .is_some_and(is_vowel);
        if w.get(i) == Some(&'y') && (i == 0 || prev_vowel) {
            if let Some(c) = w.get_mut(i) {
                *c = 'Y';
            }
        }
    }
    let p1 = region_after(&w, 0);
    let p2 = region_after(&w, p1);

    if let Some((len, action)) = longest(&w, STEP_1A) {
        match action {
            1 => replace_tail(&mut w, len, "ss"),
            2 => replace_tail(&mut w, len, "i"),
            3 => replace_tail(&mut w, len, ""),
            _ => {}
        }
    }

    if let Some((len, action)) = longest(&w, STEP_1B) {
        let stem_end = w.len() - len;
        if action == 1 {
            if p1 <= stem_end {
                replace_tail(&mut w, len, "ee");
            }
        } else if w.iter().take(stem_end).any(|c| is_vowel(*c)) {
            w.truncate(stem_end);
            match longest(&w, STEP_1B_FIX) {
                Some((_, 1)) => w.push('e'),
                Some((_, 2)) => {
                    w.pop();
                }
                _ if w.len() == p1 && short_vowel_at(&w, w.len()) => w.push('e'),
                _ => {}
            }
        }
    }

    if matches!(w.last(), Some('y' | 'Y')) && w.iter().take(w.len() - 1).any(|c| is_vowel(*c)) {
        replace_tail(&mut w, 1, "i");
    }

    if let Some((len, with)) = longest_pair(&w, STEP_2)
        && p1 <= w.len() - len
    {
        replace_tail(&mut w, len, with);
    }
    if let Some((len, with)) = longest_pair(&w, STEP_3)
        && p1 <= w.len() - len
    {
        replace_tail(&mut w, len, with);
    }

    let keyed: Vec<(&str, u8)> = STEP_4
        .iter()
        .map(|s| (*s, u8::from(*s == "ion") + 1))
        .collect();
    if let Some((len, action)) = longest(&w, &keyed) {
        let bra = w.len() - len;
        let after_st = bra
            .checked_sub(1)
            .and_then(|i| w.get(i))
            .is_some_and(|c| matches!(c, 's' | 't'));
        if p2 <= bra && (action == 1 || after_st) {
            w.truncate(bra);
        }
    }

    if w.last() == Some(&'e') {
        let bra = w.len() - 1;
        if p2 <= bra || (p1 <= bra && !short_vowel_at(&w, bra)) {
            w.pop();
        }
    }
    if w.ends_with(&['l', 'l']) && p2 < w.len() {
        w.pop();
    }

    w.into_iter()
        .map(|c| if c == 'Y' { 'y' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::stem;

    #[test]
    fn matches_known_porter_outputs() {
        for (word, want) in [
            ("odyssey", "odyssei"),
            ("mythology", "mythologi"),
            ("one", "on"),
            ("tres", "tre"),
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("relational", "relat"),
            ("generalization", "gener"),
            ("hopping", "hop"),
            ("filing", "file"),
            ("happy", "happi"),
            ("controll", "control"),
            ("cups", "cup"),
        ] {
            assert_eq!(stem(word), want, "{word}");
        }
    }
}
