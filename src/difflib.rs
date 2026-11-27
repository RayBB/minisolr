//! A port of the parts of Python's `difflib` that Python minisolr's spellcheck uses:
//! `SequenceMatcher` (`find_longest_match`, `get_matching_blocks`, `ratio`,
//! `quick_ratio`, `real_quick_ratio`) over the characters of two strings, with no junk
//! function and the default `autojunk=True`, and `get_close_matches`.
//!
//! The matching follows `CPython`'s algorithm step for step (the same longest-match tie
//! breaks and the same recursion order), so ratios and therefore suggestions and their
//! order are the same as Python's.

use std::collections::HashMap;

/// `difflib.SequenceMatcher(None, a, b)` over characters.
#[derive(Debug, Clone)]
pub struct SequenceMatcher {
    a: Vec<char>,
    b: Vec<char>,
    /// For each character of `b` that is not "popular", its indices in `b`, ascending.
    b2j: HashMap<char, Vec<usize>>,
    /// Character counts of `b`, for `quick_ratio`.
    b_counts: HashMap<char, usize>,
}

/// A matching block: `a[i..i + size] == b[j..j + size]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    /// Start in `a`.
    pub i: usize,
    /// Start in `b`.
    pub j: usize,
    /// Length.
    pub size: usize,
}

impl SequenceMatcher {
    /// A matcher of `a` against `b`.
    #[must_use]
    pub fn new(a: &str, b: &str) -> Self {
        let mut m = Self {
            a: Vec::new(),
            b: Vec::new(),
            b2j: HashMap::new(),
            b_counts: HashMap::new(),
        };
        m.set_seq2(b);
        m.set_seq1(a);
        m
    }

    /// Replaces `a` (the cached data about `b` is kept, as in Python).
    pub fn set_seq1(&mut self, a: &str) {
        self.a = a.chars().collect();
    }

    /// Replaces `b` and rebuilds `b2j`; with 200 or more characters, characters occurring
    /// more than `len / 100 + 1` times are dropped from it (Python's autojunk heuristic).
    pub fn set_seq2(&mut self, b: &str) {
        self.b = b.chars().collect();
        self.b2j.clear();
        self.b_counts.clear();
        for (j, &c) in self.b.iter().enumerate() {
            self.b2j.entry(c).or_default().push(j);
            *self.b_counts.entry(c).or_default() += 1;
        }
        let n = self.b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            self.b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
    }

    /// The longest matching block in `a[alo..ahi]` and `b[blo..bhi]`; among equally long
    /// ones the one starting earliest in `a`, then earliest in `b`.
    #[must_use]
    pub fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Match {
        let (mut best_a, mut best_b, mut best_size) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut new_j2len: HashMap<usize, usize> = HashMap::new();
            let Some(c) = self.a.get(i) else {
                break;
            };
            if let Some(js) = self.b2j.get(c) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|p| j2len.get(&p))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    new_j2len.insert(j, k);
                    if k > best_size {
                        best_a = i + 1 - k;
                        best_b = j + 1 - k;
                        best_size = k;
                    }
                }
            }
            j2len = new_j2len;
        }
        // Extend over equal neighbours that b2j left out (popular characters); there is no
        // junk function, so nothing is junk.
        while best_a > alo && best_b > blo && self.a.get(best_a - 1) == self.b.get(best_b - 1) {
            best_a -= 1;
            best_b -= 1;
            best_size += 1;
        }
        while best_a + best_size < ahi
            && best_b + best_size < bhi
            && self.a.get(best_a + best_size) == self.b.get(best_b + best_size)
        {
            best_size += 1;
        }
        Match {
            i: best_a,
            j: best_b,
            size: best_size,
        }
    }

    /// The matching blocks in order, without the final empty sentinel and with adjacent
    /// blocks joined, as Python's `get_matching_blocks` (minus the sentinel).
    #[must_use]
    pub fn matching_blocks(&self) -> Vec<Match> {
        let mut queue = vec![(0, self.a.len(), 0, self.b.len())];
        let mut blocks = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.find_longest_match(alo, ahi, blo, bhi);
            if m.size > 0 {
                blocks.push(m);
                if alo < m.i && blo < m.j {
                    queue.push((alo, m.i, blo, m.j));
                }
                if m.i + m.size < ahi && m.j + m.size < bhi {
                    queue.push((m.i + m.size, ahi, m.j + m.size, bhi));
                }
            }
        }
        blocks.sort_unstable();
        let mut joined: Vec<Match> = Vec::with_capacity(blocks.len());
        for m in blocks {
            match joined.last_mut() {
                Some(last) if last.i + last.size == m.i && last.j + last.size == m.j => {
                    last.size += m.size;
                }
                _ => joined.push(m),
            }
        }
        joined
    }

    fn ratio_of(&self, matches: usize) -> f64 {
        let total = self.a.len() + self.b.len();
        if total == 0 {
            1.0
        } else {
            2.0 * to_f64(matches) / to_f64(total)
        }
    }

    /// `2 * M / T`, where `M` is the number of matched characters and `T` the total
    /// number of characters in both strings (1.0 for two empty strings).
    #[must_use]
    pub fn ratio(&self) -> f64 {
        self.ratio_of(self.matching_blocks().iter().map(|m| m.size).sum())
    }

    /// An upper bound on [`Self::ratio`] from the characters both strings have, ignoring
    /// order.
    #[must_use]
    pub fn quick_ratio(&self) -> f64 {
        let mut avail: HashMap<char, isize> = HashMap::new();
        let mut matches = 0;
        for c in &self.a {
            let left = avail.entry(*c).or_insert_with(|| {
                isize::try_from(self.b_counts.get(c).copied().unwrap_or(0)).unwrap_or(isize::MAX)
            });
            *left -= 1;
            if *left >= 0 {
                matches += 1;
            }
        }
        self.ratio_of(matches)
    }

    /// An upper bound on [`Self::quick_ratio`] from the lengths alone.
    #[must_use]
    pub fn real_quick_ratio(&self) -> f64 {
        self.ratio_of(self.a.len().min(self.b.len()))
    }
}

fn to_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// `difflib.get_close_matches(word, possibilities, n, cutoff)`: the at most `n`
/// possibilities whose ratio against `word` is at least `cutoff`, best first; equal
/// ratios put the larger string first (Python ranks `(ratio, string)` tuples).
#[must_use]
pub fn get_close_matches<'a>(
    word: &str,
    possibilities: impl IntoIterator<Item = &'a str>,
    n: usize,
    cutoff: f64,
) -> Vec<&'a str> {
    let mut s = SequenceMatcher::new("", word);
    let mut scored: Vec<(f64, &str)> = Vec::new();
    for x in possibilities {
        s.set_seq1(x);
        if s.real_quick_ratio() >= cutoff && s.quick_ratio() >= cutoff {
            let r = s.ratio();
            if r >= cutoff {
                scored.push((r, x));
            }
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(a.1)));
    scored.truncate(n);
    scored.into_iter().map(|(_, x)| x).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio(a: &str, b: &str) -> f64 {
        SequenceMatcher::new(a, b).ratio()
    }

    #[test]
    fn ratios_match_python() {
        // Values printed by CPython 3.14's difflib.
        assert!((ratio("abcd", "bcde") - 0.75).abs() < 1e-12);
        assert!((ratio("twian", "twain") - 0.8).abs() < 1e-12);
        assert!((ratio("", "") - 1.0).abs() < 1e-12);
        assert!((ratio("abc", "") - 0.0).abs() < 1e-12);
        // Not symmetric: the greedy longest-match recursion depends on the roles.
        assert!((ratio("tide", "diet") - 0.25).abs() < 1e-12);
        assert!((ratio("diet", "tide") - 0.5).abs() < 1e-12);
        let qbf = SequenceMatcher::new(
            "private Thread currentThread;",
            "private volatile Thread currentThread;",
        );
        assert!((qbf.ratio() - 0.865_671_641_791_044_7).abs() < 1e-12);
    }

    #[test]
    fn matching_blocks_match_python() {
        let m = SequenceMatcher::new("abxcd", "abcd");
        assert_eq!(
            m.matching_blocks(),
            [
                Match {
                    i: 0,
                    j: 0,
                    size: 2
                },
                Match {
                    i: 3,
                    j: 2,
                    size: 2
                }
            ]
        );
        let m = SequenceMatcher::new(" abcd", "abcd abcd");
        assert_eq!(
            m.find_longest_match(0, 5, 0, 9),
            Match {
                i: 0,
                j: 4,
                size: 5
            }
        );
    }

    #[test]
    fn close_matches_match_python() {
        let words = ["ape", "apple", "peach", "puppy"];
        assert_eq!(get_close_matches("appel", words, 3, 0.6), ["apple", "ape"]);
        let kw = [
            "and", "as", "assert", "break", "class", "continue", "def", "del", "elif", "else",
            "except", "exec", "finally", "for", "from", "global", "if", "import", "in", "is",
            "lambda", "not", "or", "pass", "print", "raise", "return", "try", "while", "with",
            "yield",
        ];
        assert_eq!(get_close_matches("wheel", kw, 3, 0.6), ["while"]);
        assert_eq!(get_close_matches("accept", kw, 3, 0.6), ["except"]);
        // Equal ratios: the larger string first.
        assert_eq!(
            get_close_matches("ab", ["ac", "ad", "aa"], 3, 0.5),
            ["ad", "ac", "aa"]
        );
        assert_eq!(get_close_matches("ab", ["ac", "ad", "aa"], 1, 0.5), ["ad"]);
    }

    #[test]
    fn autojunk_drops_popular_characters_from_long_b() {
        let b = "a".repeat(200) + "b";
        let m = SequenceMatcher::new("ab", &b);
        // 'a' is popular in b, so only 'b' is found first, then extended backwards.
        assert_eq!(
            m.find_longest_match(0, 2, 0, 201),
            Match {
                i: 0,
                j: 199,
                size: 2
            }
        );
    }
}
