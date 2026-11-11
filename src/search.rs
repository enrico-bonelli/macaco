//! Search query compilation. Plain text searches are compiled to an escaped
//! regular expression so that every mode shares one code path.

use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, Default)]
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
}
