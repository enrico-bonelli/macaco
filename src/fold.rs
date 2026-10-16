//! Code folding: the blocks of a document that can be collapsed so that
//! only their first line (a function signature, a class header, …) stays
//! visible.
//!
//! Blocks are found with brackets (`{}`, `()`, `[]` outside strings and
//! comments) in C-like languages and with indentation in Python, YAML and
//! HTML.

use crate::lang::Lang;
use crate::lexer::{self, Kind, State};

/// A foldable block: line `header` stays visible, lines `header + 1 ..=
/// last` are hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub header: u32,
    pub last: u32,
    /// A code block (`{}` or indentation) rather than a list of arguments
    /// or items (`()`, `[]`).
    pub block: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Brackets,
    Indent,
}

fn style(lang: Lang) -> Option<Style> {
    match lang {
        Lang::C
        | Lang::Cpp
        | Lang::Rust
        | Lang::Php
        | Lang::Bash
        | Lang::JavaScript
        | Lang::TypeScript
        | Lang::Css
        | Lang::Java
        | Lang::Go
        | Lang::Vue => Some(Style::Brackets),
        Lang::Python | Lang::Yaml | Lang::Html => Some(Style::Indent),
        Lang::Plain | Lang::Markdown | Lang::Sql => None,
    }
}

/// Whether documents in `lang` can be folded.
pub fn supported(lang: Lang) -> bool {
    style(lang).is_some()
}

/// Foldable blocks of `text`, sorted by header; when several blocks start
/// on the same line only the largest is kept.
pub fn ranges(lang: Lang, text: &str) -> Vec<Range> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = match style(lang) {
        Some(Style::Brackets) => brackets(lang, &lines),
        Some(Style::Indent) => indent(lang, &lines),
        None => Vec::new(),
    };
    out.sort_by(|a, b| a.header.cmp(&b.header).then(b.last.cmp(&a.last)));
    out.dedup_by_key(|r| r.header);
    out
}

fn first_non_blank(line: &str) -> Option<usize> {
    line.bytes().position(|c| !c.is_ascii_whitespace())
}

fn brackets(lang: Lang, lines: &[&str]) -> Vec<Range> {
    let mut out = Vec::new();
    let mut state = lexer::initial_state(lang);
    let mut toks = Vec::new();
    // (opening bracket, line, whether it is the first thing on its line)
    let mut stack: Vec<(u8, usize, bool)> = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        state = lexer::lex_line(lang, state, line, &mut toks);
        let first = first_non_blank(line);
        let mut ti = 0;
        for (i, c) in line.bytes().enumerate() {
            let open = match c {
                b'{' | b'(' | b'[' => None,
                b'}' => Some(b'{'),
                b')' => Some(b'('),
                b']' => Some(b'['),
                _ => continue,
            };
            while ti < toks.len() && toks[ti].end as usize <= i {
                ti += 1;
            }
            if toks.get(ti).is_some_and(|t| t.start as usize <= i && matches!(t.kind, Kind::Str | Kind::Comment)) {
                continue;
            }
            let Some(open) = open else {
                stack.push((c, n, first == Some(i)));
                continue;
            };
            let Some(pos) = stack.iter().rposition(|e| e.0 == open) else { continue };
            let (_, a, alone) = stack[pos];
            stack.truncate(pos);
            // A closing bracket that starts its line stays visible.
            let last = if first == Some(i) { n.saturating_sub(1) } else { n };
            // Allman style: a lone `{` belongs to the line above.
            let header = if open == b'{' && alone && a > 0 && {
                let prev = lines[a - 1].trim_end();
                !prev.trim_start().is_empty() && !prev.ends_with([';', '{', '}'])
            } {
                a - 1
            } else {
                a
            };
            if last > a {
                out.push(Range { header: header as u32, last: last as u32, block: open == b'{' });
            }
        }
    }
    out
}

fn indent_width(line: &str) -> usize {
    let mut w = 0;
    for c in line.bytes() {
        match c {
            b' ' => w += 1,
            b'\t' => w = (w / 4 + 1) * 4,
            _ => break,
        }
    }
    w
}

fn indent(lang: Lang, lines: &[&str]) -> Vec<Range> {
    let mut out = Vec::new();
    let mut state = lexer::initial_state(lang);
    let mut toks = Vec::new();
    // (indentation, header line)
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut last_code = 0;
    let comment = if lang == Lang::Html { None } else { Some('#') };
    let mut close = |stack: &mut Vec<(usize, usize)>, below: Option<usize>, last_code: usize| {
        while let Some(&(w, h)) = stack.last() {
            if below.is_some_and(|b| b > w) {
                break;
            }
            stack.pop();
            if last_code > h {
                out.push(Range { header: h as u32, last: last_code as u32, block: true });
            }
        }
    };
    for (n, line) in lines.iter().enumerate() {
        // Lines inside a multi-line string do not count.
        let inside = lang == Lang::Python && state != State::Normal;
        if lang == Lang::Python {
            state = lexer::lex_line(lang, state, line, &mut toks);
        }
        let t = line.trim_start();
        if inside || t.is_empty() || comment.is_some_and(|c| t.starts_with(c)) {
            continue;
        }
        let w = indent_width(line);
        close(&mut stack, Some(w), last_code);
        stack.push((w, n));
        last_code = n;
    }
    close(&mut stack, None, last_code);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(lang: Lang, text: &str) -> Vec<(u32, u32)> {
        ranges(lang, text).iter().map(|r| (r.header, r.last)).collect()
    }

    #[test]
    fn braces() {
        let src = "fn a() {\n    let s = \"{\";\n    if x {\n        y();\n    }\n}\nfn b() {}\n";
        assert_eq!(spans(Lang::Rust, src), vec![(0, 4), (2, 3)]);
    }

    #[test]
    fn allman_and_else() {
        let src = "void f()\n{\n    a();\n}\nif (x) {\n    a();\n} else {\n    b();\n}\n";
        assert_eq!(spans(Lang::C, src), vec![(0, 2), (4, 5), (6, 7)]);
    }

    #[test]
    fn arguments() {
        let src = "foo(a,\n    b,\n    c)\nx = [\n  1,\n  2,\n]\n";
        let r = ranges(Lang::JavaScript, src);
        assert_eq!(r.iter().map(|r| (r.header, r.last, r.block)).collect::<Vec<_>>(), vec![(0, 2, false), (3, 5, false)]);
    }

    #[test]
    fn comments_ignored() {
        let src = "/* { */\nint f() {\n    // }\n    return 0;\n}\n";
        assert_eq!(spans(Lang::C, src), vec![(1, 3)]);
    }

    #[test]
    fn python() {
        let src = "class A:\n    def f(self):\n        \"\"\"Doc\nstring\"\"\"\n        return 1\n\n    def g(self):\n        pass\n\n\nx = 1\n";
        assert_eq!(spans(Lang::Python, src), vec![(0, 7), (1, 4), (6, 7)]);
    }

    #[test]
    fn yaml_and_unsupported() {
        assert_eq!(spans(Lang::Yaml, "a:\n  b: 1\n  # c\n  d:\n    e: 2\nf: 3\n"), vec![(0, 4), (3, 4)]);
        assert!(spans(Lang::Plain, "a {\n b\n}\n").is_empty());
    }
}
