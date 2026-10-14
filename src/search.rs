//! Search query compilation. Plain text searches are compiled to an escaped
//! regular expression so that every mode shares one code path.

use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

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

    /// Replacement text for the match `m` found in `hay`.
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

/// Convert a byte offset in `s` into a char offset, continuing from a
/// previously computed (byte, char) pair to keep scans linear.
pub fn char_offset(s: &str, from: (usize, usize), byte: usize) -> usize {
    from.1 + s[from.0..byte].chars().count()
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
        assert!(Query::new("(", Options { regex: true, ..Default::default() }).is_err());
    }
}
