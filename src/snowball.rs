//! The Snowball English (Porter2) stemmer, ported from the code Snowball 3.1.1 generates
//! for `english.sbl` (the `snowballstemmer` package Python minisolr uses for every
//! `snowballPorter` filter, whatever its `language`).
//!
//! The port keeps the generated code's structure: a small runtime with a cursor, `bra` and
//! `ket` slice marks and backward limits, and one function per routine.

/// The Snowball runtime state over the characters of one word.
struct Env {
    cur: Vec<char>,
    cursor: usize,
    limit: usize,
    lb: usize,
    bra: usize,
    ket: usize,
    p1: usize,
    p2: usize,
    y_found: bool,
}

const V: &[char] = &['a', 'e', 'i', 'o', 'u', 'y'];
const V_WXY: &[char] = &['Y', 'a', 'e', 'i', 'o', 'u', 'w', 'x', 'y'];
const VALID_LI: &[char] = &['c', 'd', 'e', 'g', 'h', 'k', 'm', 'n', 'r', 't'];
const AEO: &[char] = &['a', 'e', 'o'];

const A_0: &[(&str, i32)] = &[
    ("arsen", -1),
    ("commun", -1),
    ("emerg", -1),
    ("gener", -1),
    ("inter", -1),
    ("later", -1),
    ("organ", -1),
    ("past", -1),
    ("univers", -1),
];
const A_1: &[(&str, i32)] = &[("'", 1), ("'s'", 1), ("'s", 1)];
const A_2: &[(&str, i32)] = &[
    ("ied", 2),
    ("s", 3),
    ("ies", 2),
    ("sses", 1),
    ("ss", -1),
    ("us", -1),
];
const A_3: &[(&str, i32)] = &[("succ", 1), ("proc", 1), ("exc", 1)];
const A_4: &[(&str, i32)] = &[
    ("even", 2),
    ("cann", 2),
    ("inn", 2),
    ("earr", 2),
    ("herr", 2),
    ("out", 2),
    ("y", 1),
];
const A_5: &[(&str, i32)] = &[
    ("", -1),
    ("ed", 2),
    ("eed", 1),
    ("ing", 3),
    ("edly", 2),
    ("eedly", 1),
    ("ingly", 2),
];
const A_6: &[(&str, i32)] = &[
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
const A_7: &[(&str, i32)] = &[
    ("anci", 3),
    ("enci", 2),
    ("ogi", 14),
    ("li", 16),
    ("bli", 12),
    ("abli", 4),
    ("alli", 8),
    ("fulli", 9),
    ("lessli", 15),
    ("ousli", 10),
    ("entli", 5),
    ("aliti", 8),
    ("biliti", 12),
    ("iviti", 11),
    ("tional", 1),
    ("ational", 7),
    ("alism", 8),
    ("ation", 7),
    ("ization", 6),
    ("izer", 6),
    ("ator", 7),
    ("iveness", 11),
    ("fulness", 9),
    ("ousness", 10),
    ("ogist", 13),
];
const A_8: &[(&str, i32)] = &[
    ("icate", 4),
    ("ative", 6),
    ("alize", 3),
    ("iciti", 4),
    ("ical", 4),
    ("tional", 1),
    ("ational", 2),
    ("ful", 5),
    ("ness", 5),
];
const A_9: &[(&str, i32)] = &[
    ("ic", 1),
    ("ance", 1),
    ("ence", 1),
    ("able", 1),
    ("ible", 1),
    ("ate", 1),
    ("ive", 1),
    ("ize", 1),
    ("iti", 1),
    ("al", 1),
    ("ism", 1),
    ("ion", 2),
    ("er", 1),
    ("ous", 1),
    ("ant", 1),
    ("ent", 1),
    ("ment", 1),
    ("ement", 1),
];
const A_10: &[(&str, i32)] = &[("e", 1), ("l", 2)];
const A_11: &[(&str, i32)] = &[
    ("andes", -1),
    ("atlas", -1),
    ("bias", -1),
    ("cosmos", -1),
    ("early", 6),
    ("gently", 4),
    ("howe", -1),
    ("idly", 3),
    ("news", -1),
    ("only", 7),
    ("singly", 8),
    ("skies", 2),
    ("skis", 1),
    ("sky", -1),
    ("ugly", 5),
];
const AS_11: &[&str] = &[
    "ski", "sky", "idl", "gentl", "ugli", "earli", "onli", "singl",
];

impl Env {
    fn at(&self, i: usize) -> Option<char> {
        self.cur.get(i).copied()
    }

    fn in_grouping(&mut self, s: &[char]) -> bool {
        if self.cursor >= self.limit || !self.at(self.cursor).is_some_and(|c| s.contains(&c)) {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn go_in_grouping(&mut self, s: &[char]) -> bool {
        while self.cursor < self.limit {
            if !self.at(self.cursor).is_some_and(|c| s.contains(&c)) {
                return true;
            }
            self.cursor += 1;
        }
        false
    }

    fn go_out_grouping(&mut self, s: &[char]) -> bool {
        while self.cursor < self.limit {
            if self.at(self.cursor).is_some_and(|c| s.contains(&c)) {
                return true;
            }
            self.cursor += 1;
        }
        false
    }

    fn prev(&self) -> Option<char> {
        if self.cursor <= self.lb {
            return None;
        }
        self.at(self.cursor - 1)
    }

    fn in_grouping_b(&mut self, s: &[char]) -> bool {
        if self.prev().is_some_and(|c| s.contains(&c)) {
            self.cursor -= 1;
            true
        } else {
            false
        }
    }

    fn out_grouping_b(&mut self, s: &[char]) -> bool {
        if self.prev().is_some_and(|c| !s.contains(&c)) {
            self.cursor -= 1;
            true
        } else {
            false
        }
    }

    fn go_out_grouping_b(&mut self, s: &[char]) -> bool {
        while self.cursor > self.lb {
            if self.at(self.cursor - 1).is_some_and(|c| s.contains(&c)) {
                return true;
            }
            self.cursor -= 1;
        }
        false
    }

    fn prev_is(&mut self, c: char) -> bool {
        if self.prev() == Some(c) {
            self.cursor -= 1;
            true
        } else {
            false
        }
    }

    fn eq_s_b(&mut self, s: &str) -> bool {
        let n = s.chars().count();
        if self.cursor < self.lb + n {
            return false;
        }
        let start = self.cursor - n;
        if self
            .cur
            .get(start..self.cursor)
            .is_some_and(|w| w.iter().copied().eq(s.chars()))
        {
            self.cursor = start;
            true
        } else {
            false
        }
    }

    /// The longest entry that starts at the cursor (before `limit`); moves past it.
    fn find_among(&mut self, table: &[(&str, i32)]) -> i32 {
        let mut best: Option<(usize, i32)> = None;
        for (s, result) in table {
            let n = s.chars().count();
            let fits = self.cursor + n <= self.limit
                && self
                    .cur
                    .get(self.cursor..self.cursor + n)
                    .is_some_and(|w| w.iter().copied().eq(s.chars()));
            if fits && best.is_none_or(|(len, _)| n > len) {
                best = Some((n, *result));
            }
        }
        best.map_or(0, |(n, result)| {
            self.cursor += n;
            result
        })
    }

    /// The longest entry that ends at the cursor (after `lb`); moves before it.
    fn find_among_b(&mut self, table: &[(&str, i32)]) -> i32 {
        let mut best: Option<(usize, i32)> = None;
        for (s, result) in table {
            let n = s.chars().count();
            let fits = self.cursor >= self.lb + n
                && self
                    .cur
                    .get(self.cursor - n..self.cursor)
                    .is_some_and(|w| w.iter().copied().eq(s.chars()));
            if fits && best.is_none_or(|(len, _)| n > len) {
                best = Some((n, *result));
            }
        }
        best.map_or(0, |(n, result)| {
            self.cursor -= n;
            result
        })
    }

    fn slice_from(&mut self, s: &str) {
        let (c_bra, c_ket) = (self.bra, self.ket.min(self.cur.len()));
        let new: Vec<char> = s.chars().collect();
        let old_len = c_ket.saturating_sub(c_bra);
        let new_len = new.len();
        self.cur.splice(c_bra..c_ket, new);
        if new_len >= old_len {
            self.limit += new_len - old_len;
        } else {
            self.limit -= old_len - new_len;
        }
        if self.cursor >= c_ket {
            self.cursor = self.cursor + new_len - old_len;
        } else if self.cursor > c_bra {
            self.cursor = c_bra;
        }
        self.ket = c_bra + new_len;
    }

    fn slice_del(&mut self) {
        self.slice_from("");
    }

    fn r1(&self) -> bool {
        self.p1 <= self.cursor
    }

    fn r2(&self) -> bool {
        self.p2 <= self.cursor
    }

    fn prelude(&mut self) {
        self.y_found = false;
        let v1 = self.cursor;
        self.bra = self.cursor;
        if self.cursor < self.limit && self.at(self.cursor) == Some('\'') {
            self.cursor += 1;
            self.ket = self.cursor;
            self.slice_del();
        }
        self.cursor = v1;
        let v2 = self.cursor;
        self.bra = self.cursor;
        if self.cursor < self.limit && self.at(self.cursor) == Some('y') {
            self.cursor += 1;
            self.ket = self.cursor;
            self.slice_from("Y");
            self.y_found = true;
        }
        self.cursor = v2;
        let v3 = self.cursor;
        loop {
            let v4 = self.cursor;
            let found = loop {
                let v5 = self.cursor;
                if self.in_grouping(V) {
                    self.bra = self.cursor;
                    if self.cursor < self.limit && self.at(self.cursor) == Some('y') {
                        self.cursor += 1;
                        self.ket = self.cursor;
                        self.cursor = v5;
                        break true;
                    }
                }
                self.cursor = v5;
                if self.cursor >= self.limit {
                    break false;
                }
                self.cursor += 1;
            };
            if !found {
                self.cursor = v4;
                break;
            }
            self.slice_from("Y");
            self.y_found = true;
        }
        self.cursor = v3;
    }

    fn mark_regions(&mut self) {
        self.p1 = self.limit;
        self.p2 = self.limit;
        let v1 = self.cursor;
        self.mark_regions_inner();
        self.cursor = v1;
    }

    fn mark_regions_inner(&mut self) {
        let v2 = self.cursor;
        if self.find_among(A_0) == 0 {
            self.cursor = v2;
            if !self.go_out_grouping(V) {
                return;
            }
            self.cursor += 1;
            if !self.go_in_grouping(V) {
                return;
            }
            self.cursor += 1;
        }
        self.p1 = self.cursor;
        if !self.go_out_grouping(V) {
            return;
        }
        self.cursor += 1;
        if !self.go_in_grouping(V) {
            return;
        }
        self.cursor += 1;
        self.p2 = self.cursor;
    }

    fn shortv(&mut self) -> bool {
        let v1 = self.limit - self.cursor;
        if self.out_grouping_b(V_WXY) && self.in_grouping_b(V) && self.out_grouping_b(V) {
            return true;
        }
        self.cursor = self.limit - v1;
        if self.out_grouping_b(V) && self.in_grouping_b(V) && self.cursor <= self.lb {
            return true;
        }
        self.cursor = self.limit - v1;
        self.eq_s_b("past")
    }

    fn step_1a(&mut self) -> bool {
        let v1 = self.limit - self.cursor;
        self.ket = self.cursor;
        if self.find_among_b(A_1) == 0 {
            self.cursor = self.limit - v1;
        } else {
            self.bra = self.cursor;
            self.slice_del();
        }
        self.ket = self.cursor;
        let among = self.find_among_b(A_2);
        if among == 0 {
            return false;
        }
        self.bra = self.cursor;
        match among {
            1 => self.slice_from("ss"),
            2 => {
                if self.cursor >= self.lb + 2 {
                    self.cursor -= 2;
                    self.slice_from("i");
                } else {
                    self.slice_from("ie");
                }
            }
            3 => {
                if self.cursor <= self.lb {
                    return false;
                }
                self.cursor -= 1;
                if !self.go_out_grouping_b(V) {
                    return false;
                }
                self.cursor -= 1;
                self.slice_del();
            }
            _ => {}
        }
        true
    }

    /// The first alternative of Step 1b; `false` falls through to the second.
    fn step_1b_first(&mut self, among: i32) -> bool {
        match among {
            1 => {
                let v2 = self.limit - self.cursor;
                if self.r1() {
                    let v3 = self.limit - self.cursor;
                    if !(self.find_among_b(A_3) != 0 && self.cursor <= self.lb) {
                        self.cursor = self.limit - v3;
                        self.slice_from("ee");
                    }
                }
                self.cursor = self.limit - v2;
                true
            }
            2 => false,
            3 => match self.find_among_b(A_4) {
                0 => false,
                1 => {
                    let v4 = self.limit - self.cursor;
                    if !self.out_grouping_b(V) || self.cursor > self.lb {
                        return false;
                    }
                    self.cursor = self.limit - v4;
                    self.bra = self.cursor;
                    self.slice_from("ie");
                    true
                }
                _ => self.cursor <= self.lb,
            },
            _ => true,
        }
    }

    fn step_1b(&mut self) -> bool {
        self.ket = self.cursor;
        let among = self.find_among_b(A_5);
        self.bra = self.cursor;
        let v1 = self.limit - self.cursor;
        if self.step_1b_first(among) {
            return true;
        }
        self.cursor = self.limit - v1;
        let v5 = self.limit - self.cursor;
        if !self.go_out_grouping_b(V) {
            return false;
        }
        self.cursor = self.limit - v5;
        self.slice_del();
        self.ket = self.cursor;
        self.bra = self.cursor;
        let v6 = self.limit - self.cursor;
        match self.find_among_b(A_6) {
            1 => {
                self.slice_from("e");
                return false;
            }
            2 => {
                let v7 = self.limit - self.cursor;
                if self.in_grouping_b(AEO) && self.cursor <= self.lb {
                    return false;
                }
                self.cursor = self.limit - v7;
            }
            _ => {
                if self.cursor != self.p1 {
                    return false;
                }
                let v8 = self.limit - self.cursor;
                if !self.shortv() {
                    return false;
                }
                self.cursor = self.limit - v8;
                self.slice_from("e");
                return false;
            }
        }
        self.cursor = self.limit - v6;
        self.ket = self.cursor;
        if self.cursor <= self.lb {
            return false;
        }
        self.cursor -= 1;
        self.bra = self.cursor;
        self.slice_del();
        true
    }

    fn step_1c(&mut self) -> bool {
        self.ket = self.cursor;
        if !self.prev_is('y') && !self.prev_is('Y') {
            return false;
        }
        self.bra = self.cursor;
        if !self.out_grouping_b(V) || self.cursor <= self.lb {
            return false;
        }
        self.slice_from("i");
        true
    }

    fn step_2(&mut self) -> bool {
        self.ket = self.cursor;
        let among = self.find_among_b(A_7);
        if among == 0 {
            return false;
        }
        self.bra = self.cursor;
        if !self.r1() {
            return false;
        }
        let to = match among {
            1 => "tion",
            2 => "ence",
            3 => "ance",
            4 => "able",
            5 => "ent",
            6 => "ize",
            7 => "ate",
            8 => "al",
            9 => "ful",
            10 => "ous",
            11 => "ive",
            12 => "ble",
            13 => "og",
            14 => {
                if !self.prev_is('l') {
                    return false;
                }
                "og"
            }
            15 => "less",
            _ => {
                if !self.in_grouping_b(VALID_LI) {
                    return false;
                }
                ""
            }
        };
        self.slice_from(to);
        true
    }

    fn step_3(&mut self) -> bool {
        self.ket = self.cursor;
        let among = self.find_among_b(A_8);
        if among == 0 {
            return false;
        }
        self.bra = self.cursor;
        if !self.r1() {
            return false;
        }
        let to = match among {
            1 => "tion",
            2 => "ate",
            3 => "al",
            4 => "ic",
            5 => "",
            _ => {
                if !self.r2() {
                    return false;
                }
                ""
            }
        };
        self.slice_from(to);
        true
    }

    fn step_4(&mut self) -> bool {
        self.ket = self.cursor;
        let among = self.find_among_b(A_9);
        if among == 0 {
            return false;
        }
        self.bra = self.cursor;
        if !self.r2() {
            return false;
        }
        if among != 1 && !self.prev_is('s') && !self.prev_is('t') {
            return false;
        }
        self.slice_del();
        true
    }

    fn step_5(&mut self) -> bool {
        self.ket = self.cursor;
        let among = self.find_among_b(A_10);
        if among == 0 {
            return false;
        }
        self.bra = self.cursor;
        if among == 1 {
            if !self.r2() {
                if !self.r1() {
                    return false;
                }
                let v1 = self.limit - self.cursor;
                if self.shortv() {
                    return false;
                }
                self.cursor = self.limit - v1;
            }
        } else if !self.r2() || !self.prev_is('l') {
            return false;
        }
        self.slice_del();
        true
    }

    fn exception1(&mut self) -> bool {
        self.bra = self.cursor;
        let among = self.find_among(A_11);
        if among == 0 {
            return false;
        }
        self.ket = self.cursor;
        if self.cursor < self.limit {
            return false;
        }
        if let Some(to) = usize::try_from(among - 1).ok().and_then(|i| AS_11.get(i)) {
            self.slice_from(to);
        }
        true
    }

    fn postlude(&mut self) {
        if !self.y_found {
            return;
        }
        loop {
            let v1 = self.cursor;
            let found = loop {
                let v2 = self.cursor;
                self.bra = self.cursor;
                if self.cursor < self.limit && self.at(self.cursor) == Some('Y') {
                    self.cursor += 1;
                    self.ket = self.cursor;
                    self.cursor = v2;
                    break true;
                }
                self.cursor = v2;
                if self.cursor >= self.limit {
                    break false;
                }
                self.cursor += 1;
            };
            if !found {
                self.cursor = v1;
                break;
            }
            self.slice_from("y");
        }
    }

    fn stem(&mut self) {
        let v1 = self.cursor;
        if self.exception1() {
            return;
        }
        self.cursor = v1;
        if self.cursor + 3 > self.limit {
            return;
        }
        self.prelude();
        self.mark_regions();
        self.lb = self.cursor;
        self.cursor = self.limit;
        for step in [
            Self::step_1a,
            Self::step_1b,
            Self::step_1c,
            Self::step_2,
            Self::step_3,
            Self::step_4,
            Self::step_5,
        ] {
            let v = self.limit - self.cursor;
            step(self);
            self.cursor = self.limit - v;
        }
        self.cursor = self.lb;
        let v9 = self.cursor;
        self.postlude();
        self.cursor = v9;
    }
}

/// Stems `word` with Snowball English (Porter2).
#[must_use]
pub fn stem(word: &str) -> String {
    let cur: Vec<char> = word.chars().collect();
    let limit = cur.len();
    let mut env = Env {
        cur,
        cursor: 0,
        limit,
        lb: 0,
        bra: 0,
        ket: limit,
        p1: 0,
        p2: 0,
        y_found: false,
    };
    env.stem();
    env.cur.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::stem;

    #[test]
    fn stems_like_snowballstemmer_english() {
        // Outputs of snowballstemmer 3.1.1's EnglishStemmer.
        for (w, s) in [
            ("consign", "consign"),
            ("consigned", "consign"),
            ("consignment", "consign"),
            ("generously", "generous"),
            ("generalizations", "general"),
            ("knightly", "knight"),
            ("skies", "sky"),
            ("dying", "die"),
            ("lying", "lie"),
            ("news", "news"),
            ("cried", "cri"),
            ("ties", "tie"),
            ("hopping", "hop"),
            ("hoped", "hope"),
            ("failed", "fail"),
            ("succeeded", "succeed"),
            ("yelling", "yell"),
            ("sayings", "say"),
            ("abilities", "abil"),
            ("odyssey", "odyssey"),
            ("the", "the"),
            ("is", "is"),
            ("archaeology", "archaeolog"),
            ("geologist", "geolog"),
            ("'tis", "tis"),
            ("youth's", "youth"),
            ("война", "война"),
        ] {
            assert_eq!(stem(w), s, "{w}");
        }
    }
}
