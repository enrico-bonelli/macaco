//! Search query compilation. Plain text searches are compiled to an escaped
//! regular expression so that every mode shares one code path.

use regex::{Regex, RegexBuilder};
use std::cell::Cell;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone)]
pub struct Query {
    pub re: Regex,
    pub opts: Options,
}

impl Query {
    pub fn new(pattern: &str, opts: Options) -> Result<Query, String> {
        if pattern.is_empty() {
            return Err(String::new());
        }
        let mut p = if opts.regex { pattern.to_string() } else { regex::escape(pattern) };
        if opts.whole_word {
            p = format!(r"\b(?:{p})\b");
        }
        let re = RegexBuilder::new(&p)
            .case_insensitive(!opts.case_sensitive)
            .multi_line(true)
            .size_limit(1 << 22)
            .build()
            .map_err(|e| e.to_string().lines().last().unwrap_or("regex non valida").to_string())?;
        Ok(Query { re, opts })
    }

    /// Replacement text for the match starting at byte `start` of `hay`.
    pub fn replacement(&self, hay: &str, start: usize, repl: &str) -> String {
        if !self.opts.regex {
            return repl.to_string();
        }
        match self.re.captures_at(hay, start) {
            Some(caps) => {
                let mut out = String::new();
                caps.expand(repl, &mut out);
                out
            }
            None => repl.to_string(),
        }
    }
}

/// Do not keep more match positions than this (8 bytes each); later matches
/// are only counted.
pub const MAX_HITS: usize = 1 << 21;

/// The matches of a query in a text as character offsets, so that moving
/// between matches and painting them does not copy and search the text
/// again until it changes.
pub struct Hits {
    /// Compiled pattern and options of the query.
    pattern: String,
    opts: Options,
    /// Edit generation of the text searched.
    pub generation: u64,
    /// The first `MAX_HITS` non-empty matches, (start, end) in characters,
    /// sorted and not overlapping.
    spans: Vec<(u32, u32)>,
    /// Number of non-empty matches.
    pub count: usize,
    /// Characters whose matches are painted (empty: none).
    pub painted: Cell<(u32, u32)>,
}

impl Hits {
    pub fn new(q: &Query, text: &str, generation: u64) -> Hits {
        let ascii = text.is_ascii();
        let mut spans = Vec::new();
        let mut count = 0;
        let mut last = (0usize, 0usize);
        for m in q.re.find_iter(text) {
            if m.start() == m.end() {
                continue;
            }
            count += 1;
            if spans.len() < MAX_HITS {
                let (s, e) = if ascii {
                    (m.start(), m.end())
                } else {
                    let s = char_offset(text, last, m.start());
                    let e = s + text[m.start()..m.end()].chars().count();
                    last = (m.end(), e);
                    (s, e)
                };
                spans.push((s as u32, e as u32));
            }
        }
        Hits { pattern: q.re.as_str().to_string(), opts: q.opts, generation, spans, count, painted: Cell::new((0, 0)) }
    }

    /// Whether these are the matches of `q` in the text at `generation`.
    pub fn is_for(&self, q: &Query, generation: u64) -> bool {
        self.generation == generation && self.opts == q.opts && self.pattern == q.re.as_str()
    }

    fn complete(&self) -> bool {
        self.spans.len() == self.count
    }

    /// Whether every match starting before character `at` is kept: the
    /// kept ones are the first, the others start after the last kept.
    fn known_before(&self, at: u32) -> bool {
        self.complete() || self.spans.last().is_some_and(|m| at <= m.1)
    }

    /// First match starting at or after character `pivot`, else the first
    /// one. None when it may be among the matches not kept.
    pub fn next(&self, pivot: u32) -> Option<Option<(u32, u32)>> {
        let i = self.spans.partition_point(|s| s.0 < pivot);
        match self.spans.get(i) {
            Some(&s) => Some(Some(s)),
            None if self.complete() => Some(self.spans.first().copied()),
            None => None,
        }
    }

    /// Last match starting before character `pivot`, else the last one.
    /// None when it may be among the matches not kept.
    pub fn prev(&self, pivot: u32) -> Option<Option<(u32, u32)>> {
        let i = self.spans.partition_point(|s| s.0 < pivot);
        if !self.known_before(pivot) {
            return None;
        }
        if i > 0 {
            return Some(Some(self.spans[i - 1]));
        }
        self.complete().then(|| self.spans.last().copied())
    }

    /// Whether characters `s..e` are exactly a match; None when unknown
    /// (past the matches kept).
    pub fn is_match(&self, s: u32, e: u32) -> Option<bool> {
        match self.spans.binary_search_by_key(&s, |m| m.0) {
            Ok(i) => Some(self.spans[i].1 == e),
            Err(_) if !self.known_before(s + 1) => None,
            Err(_) => Some(false),
        }
    }

    /// Kept matches overlapping characters `a..b`.
    pub fn within(&self, a: u32, b: u32) -> &[(u32, u32)] {
        let lo = self.spans.partition_point(|m| m.1 <= a);
        let hi = self.spans.partition_point(|m| m.0 < b).max(lo);
        &self.spans[lo..hi]
    }
}

/// Every non-empty match of `q` in `hay` with its replacement:
/// (start, end, text). The regex runs once per match.
pub fn replacements(q: &Query, hay: &str, repl: &str) -> Vec<(usize, usize, String)> {
    if !q.opts.regex {
        return q.re.find_iter(hay).filter(|m| m.start() != m.end()).map(|m| (m.start(), m.end(), repl.to_string())).collect();
    }
    q.re
        .captures_iter(hay)
        .filter_map(|caps| {
            let m = caps.get(0)?;
            (m.start() != m.end()).then(|| {
                let mut out = String::new();
                caps.expand(repl, &mut out);
                (m.start(), m.end(), out)
            })
        })
        .collect()
}

/// Convert a byte offset in `s` into a char offset, continuing from a
/// previously computed (byte, char) pair to keep scans linear.
pub fn char_offset(s: &str, from: (usize, usize), byte: usize) -> usize {
    from.1 + s[from.0..byte].chars().count()
}

/// Byte offset of character `ch` in `s` (`s.len()` if past the end).
pub fn byte_at_char(s: &str, ch: usize) -> usize {
    if ch == 0 {
        return 0;
    }
    if s.is_ascii() {
        return ch.min(s.len());
    }
    s.char_indices().nth(ch).map_or(s.len(), |(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        let q = Query::new("a.b", Options::default()).unwrap();
        assert!(q.re.is_match("xA.Bx"));
        assert!(!q.re.is_match("axb"));
        let q = Query::new("a.b", Options { regex: true, case_sensitive: true, ..Default::default() }).unwrap();
        assert!(q.re.is_match("axb"));
        assert!(!q.re.is_match("AXB"));
        let q = Query::new("cat", Options { whole_word: true, ..Default::default() }).unwrap();
        assert!(!q.re.is_match("concatenate"));
        assert!(q.re.is_match("a cat."));
        let q = Query::new(r"(\w+)@(\w+)", Options { regex: true, ..Default::default() }).unwrap();
        assert_eq!(q.replacement("x foo@bar", 2, "$2 at $1"), "bar at foo");
        assert_eq!(replacements(&q, "a@b c@d", "$2$1"), vec![(0, 3, "ba".to_string()), (4, 7, "dc".to_string())]);
        let plain = Query::new("$1", Options::default()).unwrap();
        assert_eq!(replacements(&plain, "x$1", "$0"), vec![(1, 3, "$0".to_string())]);
        assert!(Query::new("(", Options { regex: true, ..Default::default() }).is_err());
        assert_eq!(byte_at_char("abcdef", 0), 0);
        assert_eq!(byte_at_char("abcdef", 3), 3);
        assert_eq!(byte_at_char("abcdef", 99), 6);
        let utf = "aè🦀z";
        assert_eq!(byte_at_char(utf, 0), 0);
        assert_eq!(byte_at_char(utf, 1), 1);
        assert_eq!(byte_at_char(utf, 2), 3);
        assert_eq!(byte_at_char(utf, 3), 7);
        assert_eq!(byte_at_char(utf, 9), utf.len());
    }

    #[test]
    fn hits() {
        let q = Query::new("ab", Options::default()).unwrap();
        // Characters: "è" is one, the matches are at 1..3, 4..6, 8..10.
        let h = Hits::new(&q, "èab-AB-xab", 7);
        assert_eq!(h.count, 3);
        assert!(h.is_for(&q, 7));
        assert!(!h.is_for(&q, 8));
        assert!(!h.is_for(&Query::new("ab", Options { case_sensitive: true, ..Default::default() }).unwrap(), 7));
        assert_eq!(h.next(0), Some(Some((1, 3))));
        assert_eq!(h.next(3), Some(Some((4, 6))));
        assert_eq!(h.next(9), Some(Some((1, 3))));
        assert_eq!(h.prev(4), Some(Some((1, 3))));
        assert_eq!(h.prev(5), Some(Some((4, 6))));
        assert_eq!(h.prev(1), Some(Some((8, 10))));
        assert_eq!(h.is_match(4, 6), Some(true));
        assert_eq!(h.is_match(4, 5), Some(false));
        assert_eq!(h.is_match(5, 6), Some(false));
        assert_eq!(h.within(0, 2), &[(1, 3)]);
        assert_eq!(h.within(3, 9), &[(4, 6), (8, 10)]);
        assert!(h.within(6, 8).is_empty());
        let none = Hits::new(&q, "xyz", 0);
        assert_eq!((none.count, none.next(0), none.prev(0)), (0, Some(None), Some(None)));
        // Only the first matches kept: what lies after them is unknown.
        let mut part = Hits::new(&q, "ab ab ab", 0);
        part.spans.truncate(2);
        assert_eq!(part.next(2), Some(Some((3, 5))));
        assert_eq!(part.next(4), None);
        assert_eq!(part.prev(4), Some(Some((3, 5))));
        assert_eq!(part.prev(0), None);
        assert_eq!(part.prev(7), None);
        assert_eq!(part.is_match(6, 8), None);
        assert_eq!(part.is_match(1, 3), Some(false));
    }
}
