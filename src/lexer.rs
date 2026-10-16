//! Line oriented lexers.
//!
//! Every line is lexed independently given the state at the end of the
//! previous line; the state at the end of the line is returned so the
//! highlighter can stop re-lexing as soon as states converge after an edit.
//! Token offsets are byte offsets within the line.

use crate::lang::{Lang, Spec};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Type,
    Constant,
    Number,
    Str,
    Comment,
    Preproc,
    Function,
    Variable,
    Tag,
    Attr,
    /// Markdown: headings, strong and emphasized text, links.
    Heading,
    Strong,
    Emph,
    Link,
}

pub const KINDS: [Kind; 15] = [
    Kind::Keyword,
    Kind::Type,
    Kind::Constant,
    Kind::Number,
    Kind::Str,
    Kind::Comment,
    Kind::Preproc,
    Kind::Function,
    Kind::Variable,
    Kind::Tag,
    Kind::Attr,
    Kind::Heading,
    Kind::Strong,
    Kind::Emph,
    Kind::Link,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Normal,
    /// Inside a block comment.
    Comment,
    /// Inside a string literal opened by the given quote.
    Str(u8),
    /// Inside a tripled-quote string literal.
    Triple(u8),
    /// Inside a Rust raw string with the given number of '#'.
    RawStr(u8),
    /// HTML: inside a tag, between its name and '>'.
    HtmlTag,
    /// HTML: inside <script>; payload is the encoded script state: 0-2
    /// JavaScript, 3-5 TypeScript (`lang="ts"`).
    HtmlScript(u8),
    /// HTML: inside <style>; payload is the CSS state.
    HtmlStyle(u8),
    /// CSS: low 7 bits are the brace depth, high bit = inside comment.
    Css(u8),
    /// YAML block scalar belonging to a key indented by the payload.
    YamlBlock(u8),
    /// PHP: outside of `<?php ... ?>`.
    PhpHtml,
    /// Markdown code fence: (language index, fence char, packed inner state).
    MdFence(u8, u8, u8),
}

/// Compress the state of a fenced block's language into one byte (the
/// rare states that do not fit fall back to `Normal`).
fn pack(s: State) -> u8 {
    match s {
        State::Comment => 1,
        State::Str(b'"') => 2,
        State::Str(b'\'') => 3,
        State::Str(b'`') => 4,
        State::Triple(b'"') => 5,
        State::Triple(_) => 6,
        State::RawStr(n) => 7 + n.min(8),
        State::HtmlTag => 16,
        State::PhpHtml => 17,
        State::HtmlScript(x) => 18 + x.min(5),
        State::YamlBlock(i) => 32 + i.min(31),
        State::Css(c) => 64 + (c & 0x3F) + if c & 0x80 != 0 { 64 } else { 0 },
        State::HtmlStyle(c) => 192 + (c & 0x1F) + if c & 0x80 != 0 { 32 } else { 0 },
        _ => 0,
    }
}

fn unpack(v: u8) -> State {
    match v {
        1 => State::Comment,
        2 => State::Str(b'"'),
        3 => State::Str(b'\''),
        4 => State::Str(b'`'),
        5 => State::Triple(b'"'),
        6 => State::Triple(b'\''),
        7..=15 => State::RawStr(v - 7),
        16 => State::HtmlTag,
        17 => State::PhpHtml,
        18..=23 => State::HtmlScript(v - 18),
        32..=63 => State::YamlBlock(v - 32),
        64..=127 => State::Css(v - 64),
        128..=191 => State::Css((v - 128) | 0x80),
        192..=223 => State::HtmlStyle(v - 192),
        224..=255 => State::HtmlStyle((v - 224) | 0x80),
        _ => State::Normal,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub start: u32,
    pub end: u32,
    pub kind: Kind,
}

pub fn initial_state(lang: Lang) -> State {
    match lang {
        Lang::Php => State::PhpHtml,
        Lang::Css => State::Css(0),
        _ => State::Normal,
    }
}

/// Lex one line (without its trailing newline).
pub fn lex_line(lang: Lang, state: State, line: &str, out: &mut Vec<Token>) -> State {
    out.clear();
    let b = line.as_bytes();
    let mut lx = Lexer { b, out, lang, words: word_table(lang), def_next: false };
    match lang {
        Lang::Plain => State::Normal,
        Lang::Html | Lang::Vue => lx.html(state, 0, b.len()),
        Lang::Css => {
            let st = if let State::Css(s) = state { s } else { 0 };
            State::Css(lx.css(st, 0, b.len()))
        }
        Lang::Yaml => lx.yaml(state),
        Lang::Php => lx.php(state),
        Lang::Markdown => lx.markdown(state, line),
        _ => lx.code(lang.spec(), state, 0, b.len()).0,
    }
}

fn word_table(lang: Lang) -> &'static HashMap<&'static str, Kind> {
    const N: usize = crate::lang::ALL.len();
    static TABLES: [OnceLock<HashMap<&'static str, Kind>>; N] = [const { OnceLock::new() }; N];
    let idx = crate::lang::ALL.iter().position(|&l| l == lang).unwrap_or(0);
    TABLES[idx].get_or_init(|| {
        let s = lang.spec();
        let mut m = HashMap::new();
        // Later inserts win: keywords have the highest precedence.
        for (list, kind) in [
            (s.builtins, Kind::Function),
            (s.types, Kind::Type),
            (s.constants, Kind::Constant),
            (s.keywords, Kind::Keyword),
        ] {
            for w in list {
                m.insert(*w, kind);
            }
        }
        m
    })
}

#[inline]
fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

#[inline]
fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn find(b: &[u8], from: usize, end: usize, pat: &[u8]) -> Option<usize> {
    if from >= end || pat.len() > end - from {
        return None;
    }
    b[from..end].windows(pat.len()).position(|w| w == pat).map(|p| p + from)
}

fn find_ci(b: &[u8], from: usize, end: usize, pat: &[u8]) -> Option<usize> {
    if from >= end || pat.len() > end - from {
        return None;
    }
    b[from..end]
        .windows(pat.len())
        .position(|w| w.eq_ignore_ascii_case(pat))
        .map(|p| p + from)
}

struct Lexer<'a> {
    b: &'a [u8],
    out: &'a mut Vec<Token>,
    lang: Lang,
    words: &'static HashMap<&'static str, Kind>,
    /// The previous word was `fn`/`def`/...: the next one names a function.
    def_next: bool,
}

impl Lexer<'_> {
    #[inline]
    fn tok(&mut self, start: usize, end: usize, kind: Kind) {
        if end > start {
            self.out.push(Token { start: start as u32, end: end as u32, kind });
        }
    }

    #[inline]
    fn at(&self, i: usize) -> u8 {
        self.b.get(i).copied().unwrap_or(0)
    }

    fn starts(&self, i: usize, end: usize, pat: &str) -> bool {
        let p = pat.as_bytes();
        i + p.len() <= end && &self.b[i..i + p.len()] == p
    }

    fn ident_end(&self, mut i: usize, end: usize) -> usize {
        while i < end && is_ident(self.b[i]) {
            i += 1;
        }
        i
    }

    fn number_end(&self, mut i: usize, end: usize) -> usize {
        let b = self.b;
        if b[i] == b'0' && i + 1 < end && matches!(b[i + 1], b'x' | b'X' | b'b' | b'B' | b'o' | b'O') {
            i += 2;
            while i < end && (b[i].is_ascii_hexdigit() || b[i] == b'_') {
                i += 1;
            }
        } else {
            while i < end {
                let c = b[i];
                if c.is_ascii_digit() || c == b'_' || (c == b'.' && i + 1 < end && b[i + 1].is_ascii_digit()) {
                    i += 1;
                } else if (c == b'e' || c == b'E')
                    && i + 1 < end
                    && (b[i + 1].is_ascii_digit()
                        || ((b[i + 1] == b'+' || b[i + 1] == b'-') && i + 2 < end && b[i + 2].is_ascii_digit()))
                {
                    i += 2;
                } else {
                    break;
                }
            }
        }
        // Type suffixes: 10u32, 1.5f, 10L, 3n ...
        while i < end && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Highlight `$var`, `${...}` occurrences inside [from, to).
    fn dollar_vars_in(&mut self, from: usize, to: usize) {
        let mut i = from;
        while i < to {
            if self.b[i] == b'$' {
                let e = self.dollar_var_end(i, to);
                if e > i + 1 {
                    self.tok(i, e, Kind::Variable);
                    i = e;
                    continue;
                }
            } else if self.b[i] == b'\\' {
                i += 1;
            }
            i += 1;
        }
    }

    fn dollar_var_end(&self, i: usize, end: usize) -> usize {
        let c = self.at(i + 1);
        if i + 1 >= end {
            i + 1
        } else if c == b'{' {
            find(self.b, i + 2, end, b"}").map_or(end, |p| p + 1)
        } else if is_ident_start(c) {
            self.ident_end(i + 1, end)
        } else if self.lang == Lang::Bash && (c.is_ascii_digit() || b"#?@*$!-".contains(&c)) {
            i + 2
        } else {
            i + 1
        }
    }

    /// Scan a string literal whose opening quote is at `q`; the token
    /// starts at `tok_start` (to include prefixes like `r` or `b`).
    /// Returns the position after the literal, or the state to carry over.
    fn string(&mut self, spec: &Spec, tok_start: usize, q: usize, end: usize) -> Result<usize, State> {
        let b = self.b;
        let quote = b[q];
        if spec.triple_quotes.contains(&quote) && self.at(q + 1) == quote && self.at(q + 2) == quote && q + 2 < end {
            let pat = [quote; 3];
            return match find(b, q + 3, end, &pat) {
                Some(p) => {
                    self.tok(tok_start, p + 3, Kind::Str);
                    Ok(p + 3)
                }
                None => {
                    self.tok(tok_start, end, Kind::Str);
                    Err(State::Triple(quote))
                }
            };
        }
        let raw = spec.shell && quote == b'\'';
        let mut j = q + 1;
        while j < end {
            let c = b[j];
            if c == b'\\' && !raw {
                j += 2;
                continue;
            }
            if c == quote {
                let close = j + 1;
                self.tok(tok_start, close, Kind::Str);
                if spec.dollar_vars && quote != b'\'' {
                    self.dollar_vars_in(q + 1, j);
                }
                return Ok(close);
            }
            j += 1;
        }
        self.tok(tok_start, end, Kind::Str);
        if spec.dollar_vars && quote != b'\'' {
            self.dollar_vars_in(q + 1, end);
        }
        if spec.multiline_quotes.contains(&quote) {
            Err(State::Str(quote))
        } else {
            Ok(end)
        }
    }

    /// Continue a construct that started on a previous line.
    /// Returns the position where normal lexing resumes or the state that
    /// still holds at the end of the line.
    fn resume(&mut self, spec: &Spec, state: State, i: usize, end: usize) -> Result<usize, State> {
        let b = self.b;
        match state {
            State::Comment => {
                let close = spec.block_comment.map_or("*/", |c| c.1);
                match find(b, i, end, close.as_bytes()) {
                    Some(p) => {
                        self.tok(i, p + close.len(), Kind::Comment);
                        Ok(p + close.len())
                    }
                    None => {
                        self.tok(i, end, Kind::Comment);
                        Err(State::Comment)
                    }
                }
            }
            State::Str(q) => {
                let raw = spec.shell && q == b'\'';
                let mut j = i;
                while j < end {
                    if b[j] == b'\\' && !raw {
                        j += 2;
                        continue;
                    }
                    if b[j] == q {
                        self.tok(i, j + 1, Kind::Str);
                        if spec.dollar_vars && q != b'\'' {
                            self.dollar_vars_in(i, j);
                        }
                        return Ok(j + 1);
                    }
                    j += 1;
                }
                self.tok(i, end, Kind::Str);
                if spec.dollar_vars && q != b'\'' {
                    self.dollar_vars_in(i, end);
                }
                Err(state)
            }
            State::Triple(q) => match find(b, i, end, &[q; 3]) {
                Some(p) => {
                    self.tok(i, p + 3, Kind::Str);
                    Ok(p + 3)
                }
                None => {
                    self.tok(i, end, Kind::Str);
                    Err(state)
                }
            },
            State::RawStr(n) => match self.raw_close(i, end, n as usize) {
                Some(p) => {
                    self.tok(i, p, Kind::Str);
                    Ok(p)
                }
                None => {
                    self.tok(i, end, Kind::Str);
                    Err(state)
                }
            },
            _ => Ok(i),
        }
    }

    /// End of a Rust raw string: '"' followed by n '#'.
    fn raw_close(&self, mut i: usize, end: usize, n: usize) -> Option<usize> {
        while let Some(p) = find(self.b, i, end, b"\"") {
            if p + 1 + n <= end && self.b[p + 1..p + 1 + n].iter().all(|&c| c == b'#') {
                return Some(p + 1 + n);
            }
            i = p + 1;
        }
        None
    }

    /// Generic lexer for C-like, scripting and shell languages.
    /// Returns the end state and the position where lexing stopped; a PHP
    /// closing tag `?>` stops lexing early and yields `State::PhpHtml`.
    fn code(&mut self, spec: &Spec, state: State, start: usize, end: usize) -> (State, usize) {
        let b = self.b;
        let mut i = match self.resume(spec, state, start, end) {
            Ok(p) => p,
            Err(st) => return (st, end),
        };
        let first_non_ws = b[start..end].iter().position(|c| !c.is_ascii_whitespace()).map(|p| p + start);
        let php = self.lang == Lang::Php;

        while i < end {
            let c = b[i];
            if c.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if php && c == b'?' && self.at(i + 1) == b'>' {
                self.tok(i, i + 2, Kind::Preproc);
                return (State::PhpHtml, i + 2);
            }
            if php && c == b'<' && self.at(i + 1) == b'?' {
                let e = if self.starts(i, end, "<?php") { i + 5 } else if self.at(i + 2) == b'=' { i + 3 } else { i + 2 };
                self.tok(i, e, Kind::Preproc);
                i = e;
                continue;
            }
            if let Some((open, close)) = spec.block_comment
                && self.starts(i, end, open) {
                    match find(b, i + open.len(), end, close.as_bytes()) {
                        Some(p) => {
                            self.tok(i, p + close.len(), Kind::Comment);
                            i = p + close.len();
                            continue;
                        }
                        None => {
                            self.tok(i, end, Kind::Comment);
                            return (State::Comment, end);
                        }
                    }
                }
            if php && c == b'#' && self.at(i + 1) == b'[' {
                let e = find(b, i, end, b"]").map_or(end, |p| p + 1);
                self.tok(i, e, Kind::Preproc);
                i = e;
                continue;
            }
            if spec.line_comment.iter().any(|lc| self.starts(i, end, lc))
                && (!spec.shell || i == start || b[i - 1].is_ascii_whitespace() || b";|&(".contains(&b[i - 1]))
            {
                if php {
                    // A PHP line comment ends at "?>".
                    if let Some(p) = find(b, i, end, b"?>") {
                        self.tok(i, p, Kind::Comment);
                        i = p;
                        continue;
                    }
                }
                self.tok(i, end, Kind::Comment);
                return (State::Normal, end);
            }
            if spec.preprocessor && c == b'#' && Some(i) == first_non_ws {
                let mut j = i + 1;
                while j < end && b[j] == b' ' {
                    j += 1;
                }
                let we = self.ident_end(j, end);
                self.tok(i, we, Kind::Preproc);
                let word = &b[j..we];
                i = we;
                if word == b"include" || word == b"import" {
                    while i < end && b[i] == b' ' {
                        i += 1;
                    }
                    if i < end && b[i] == b'<' {
                        let e = find(b, i, end, b">").map_or(end, |p| p + 1);
                        self.tok(i, e, Kind::Str);
                        i = e;
                    }
                }
                continue;
            }
            if spec.rust {
                if c == b'#' && (self.at(i + 1) == b'[' || (self.at(i + 1) == b'!' && self.at(i + 2) == b'[')) {
                    let mut depth = 0;
                    let mut j = i;
                    while j < end {
                        match b[j] {
                            b'[' => depth += 1,
                            b']' => {
                                depth -= 1;
                                if depth == 0 {
                                    j += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    self.tok(i, j, Kind::Preproc);
                    i = j;
                    continue;
                }
                if c == b'\'' {
                    i = self.rust_quote(i, end);
                    continue;
                }
                if c == b'r' || c == b'b' {
                    let mut j = i;
                    if b[j] == b'b' {
                        j += 1;
                    }
                    if self.at(j) == b'r' && (self.at(j + 1) == b'"' || self.at(j + 1) == b'#') {
                        let mut k = j + 1;
                        while k < end && b[k] == b'#' {
                            k += 1;
                        }
                        if self.at(k) == b'"' && k < end {
                            let n = k - j - 1;
                            match self.raw_close(k + 1, end, n) {
                                Some(p) => {
                                    self.tok(i, p, Kind::Str);
                                    i = p;
                                }
                                None => {
                                    self.tok(i, end, Kind::Str);
                                    return (State::RawStr(n as u8), end);
                                }
                            }
                            continue;
                        }
                    } else if c == b'b' && self.at(i + 1) == b'"' {
                        match self.string(spec, i, i + 1, end) {
                            Ok(p) => i = p,
                            Err(st) => return (st, end),
                        }
                        continue;
                    } else if c == b'b' && self.at(i + 1) == b'\'' {
                        i = self.rust_quote(i + 1, end);
                        if let Some(t) = self.out.last_mut().filter(|t| t.kind == Kind::Str) {
                            t.start -= 1;
                        }
                        continue;
                    }
                }
            }
            if spec.quotes.contains(&c) {
                match self.string(spec, i, i, end) {
                    Ok(p) => i = p,
                    Err(st) => return (st, end),
                }
                continue;
            }
            if spec.dollar_vars && c == b'$' {
                let e = self.dollar_var_end(i, end);
                self.tok(i, e, Kind::Variable);
                i = e;
                continue;
            }
            if spec.decorators && c == b'@' && is_ident_start(self.at(i + 1)) {
                let mut e = self.ident_end(i + 1, end);
                while self.at(e) == b'.' && is_ident_start(self.at(e + 1)) {
                    e = self.ident_end(e + 1, end);
                }
                self.tok(i, e, Kind::Preproc);
                i = e;
                continue;
            }
            if c.is_ascii_digit() || (c == b'.' && self.at(i + 1).is_ascii_digit() && !is_ident(self.at(i.wrapping_sub(1)))) {
                let e = self.number_end(i, end);
                self.tok(i, e, Kind::Number);
                i = e;
                continue;
            }
            if is_ident_start(c) {
                let e = self.ident_end(i, end);
                // Python string prefixes: r"..", f'..', rb"..".
                if self.lang == Lang::Python
                    && e - i <= 2
                    && spec.quotes.contains(&self.at(e))
                    && e < end
                    && b[i..e].iter().all(|c| b"rRbBuUfF".contains(c))
                {
                    match self.string(spec, i, e, end) {
                        Ok(p) => i = p,
                        Err(st) => return (st, end),
                    }
                    continue;
                }
                self.word(spec, i, e, end);
                i = e;
                continue;
            }
            // `func (r *T) Name()` in Go: the receiver is not the name.
            if c == b'(' {
                self.def_next = false;
            }
            i += 1;
        }
        (State::Normal, end)
    }

    fn word(&mut self, spec: &Spec, i: usize, e: usize, end: usize) {
        let b = self.b;
        let w = &b[i..e];
        let s = std::str::from_utf8(w).unwrap_or("");
        let mut kind = self.words.get(s).copied();
        if kind.is_none() && spec.case_insensitive_kw && w.iter().any(|c| c.is_ascii_uppercase()) {
            kind = self
                .words
                .get(s.to_ascii_lowercase().as_str())
                .copied()
                .filter(|k| *k == Kind::Keyword || spec.case_insensitive_words);
        }
        let def = std::mem::replace(&mut self.def_next, false);
        if let Some(k) = kind {
            self.def_next = matches!(s, "fn" | "def" | "function" | "func");
            self.tok(i, e, k);
            return;
        }
        if def {
            self.tok(i, e, Kind::Function);
            return;
        }
        // Rust macros: name!(...)
        if spec.rust && self.at(e) == b'!' && self.at(e + 1) != b'=' {
            self.tok(i, e + 1, Kind::Preproc);
            return;
        }
        let mut j = e;
        while j < end && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        if spec.shell && self.at(e) == b'=' {
            self.tok(i, e, Kind::Variable);
            return;
        }
        if self.at(j) == b'(' && !spec.shell {
            self.tok(i, e, Kind::Function);
            return;
        }
        if spec.shell && self.at(j) == b'(' && self.at(j + 1) == b')' {
            self.tok(i, e, Kind::Function);
            return;
        }
        if spec.cap_types && w[0].is_ascii_uppercase() {
            let all_caps = w.len() > 1 && w.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_');
            self.tok(i, e, if all_caps { Kind::Constant } else { Kind::Type });
        }
    }

    /// Rust: char literal or lifetime starting at the quote `i`.
    fn rust_quote(&mut self, i: usize, end: usize) -> usize {
        let b = self.b;
        if self.at(i + 1) == b'\\' {
            let e = find(b, i + 2, end, b"'").map_or(end, |p| p + 1);
            self.tok(i, e, Kind::Str);
            return e;
        }
        let lead = self.at(i + 1);
        let len = match lead {
            0..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        };
        if i + 1 + len < end && b[i + 1 + len] == b'\'' {
            self.tok(i, i + 2 + len, Kind::Str);
            return i + 2 + len;
        }
        if is_ident_start(lead) {
            let e = self.ident_end(i + 1, end);
            self.tok(i, e, Kind::Variable);
            return e;
        }
        i + 1
    }

    // ---------------------------------------------------------------- HTML

    fn html(&mut self, state: State, start: usize, end: usize) -> State {
        let b = self.b;
        let mut i = start;
        // Vue single file components: `{{ … }}` and directives.
        let vue = self.lang == Lang::Vue;
        match state {
            State::Comment => match find(b, i, end, b"-->") {
                Some(p) => {
                    self.tok(i, p + 3, Kind::Comment);
                    i = p + 3;
                }
                None => {
                    self.tok(i, end, Kind::Comment);
                    return State::Comment;
                }
            },
            State::HtmlTag => match self.tag_inner(i, end) {
                Some(p) => i = p,
                None => return State::HtmlTag,
            },
            State::HtmlScript(js) => match self.embedded_js(js, i, end) {
                Ok(p) => i = p,
                Err(st) => return st,
            },
            State::HtmlStyle(css) => match self.embedded_css(css, i, end) {
                Ok(p) => i = p,
                Err(st) => return st,
            },
            _ => {}
        }
        while i < end {
            match b[i] {
                b'<' if self.starts(i, end, "<!--") => match find(b, i + 4, end, b"-->") {
                    Some(p) => {
                        self.tok(i, p + 3, Kind::Comment);
                        i = p + 3;
                    }
                    None => {
                        self.tok(i, end, Kind::Comment);
                        return State::Comment;
                    }
                },
                b'<' if self.at(i + 1) == b'!' || self.at(i + 1) == b'?' => {
                    let e = find(b, i, end, b">").map_or(end, |p| p + 1);
                    self.tok(i, e, Kind::Preproc);
                    i = e;
                }
                b'<' if is_ident_start(self.at(i + 1)) || (self.at(i + 1) == b'/' && is_ident_start(self.at(i + 2))) => {
                    let closing = self.at(i + 1) == b'/';
                    let ns = i + 1 + closing as usize;
                    let mut ne = ns;
                    while ne < end && (is_ident(b[ne]) || b[ne] == b'-' || b[ne] == b':' || b[ne] == b'.') {
                        ne += 1;
                    }
                    self.tok(i, ne, Kind::Tag);
                    let name = &b[ns..ne];
                    let script = !closing && name.eq_ignore_ascii_case(b"script");
                    let style = !closing && name.eq_ignore_ascii_case(b"style");
                    match self.tag_inner(ne, end) {
                        Some(p) => {
                            i = p;
                            let self_closed = b[p - 2] == b'/';
                            if script && !self_closed {
                                let attrs = &b[ne..p];
                                let ts = [&b"lang=\"ts\""[..], b"lang='ts'", b"lang=\"tsx\"", b"lang='tsx'"]
                                    .iter()
                                    .any(|pat| attrs.windows(pat.len()).any(|w| w == *pat));
                                match self.embedded_js(if ts { 3 } else { 0 }, i, end) {
                                    Ok(p) => i = p,
                                    Err(st) => return st,
                                }
                            } else if style && !self_closed {
                                match self.embedded_css(0, i, end) {
                                    Ok(p) => i = p,
                                    Err(st) => return st,
                                }
                            }
                        }
                        None => return State::HtmlTag,
                    }
                }
                b'{' if vue && self.at(i + 1) == b'{' => {
                    // {{ expression }} (on one line).
                    let stop = find(b, i + 2, end, b"}}").unwrap_or(end);
                    self.tok(i, i + 2, Kind::Preproc);
                    self.embedded_expr(i + 2, stop);
                    if stop < end {
                        self.tok(stop, stop + 2, Kind::Preproc);
                        i = stop + 2;
                    } else {
                        i = end;
                    }
                }
                b'&' => {
                    let mut e = i + 1;
                    while e < end && (b[e].is_ascii_alphanumeric() || b[e] == b'#') && e - i < 32 {
                        e += 1;
                    }
                    if e < end && b[e] == b';' && e > i + 1 {
                        self.tok(i, e + 1, Kind::Constant);
                        i = e + 1;
                    } else {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
        State::Normal
    }

    /// A JavaScript expression inside a Vue template.
    fn embedded_expr(&mut self, start: usize, stop: usize) {
        let (saved_lang, saved_words) = (self.lang, self.words);
        self.lang = Lang::JavaScript;
        self.words = word_table(Lang::JavaScript);
        self.code(Lang::JavaScript.spec(), State::Normal, start, stop);
        self.lang = saved_lang;
        self.words = saved_words;
    }

    /// Attributes of a tag; returns the position after '>' if the tag
    /// closes on this line.
    fn tag_inner(&mut self, mut i: usize, end: usize) -> Option<usize> {
        let b = self.b;
        let vue = self.lang == Lang::Vue;
        while i < end {
            let c = b[i];
            // Vue directives: v-if, :prop, @event, #slot; their value is code.
            let directive = vue
                && (matches!(c, b':' | b'@' | b'#') && is_ident_start(self.at(i + 1))
                    || (c == b'v' && self.at(i + 1) == b'-' && (i == 0 || !is_ident(b[i - 1]))));
            if directive {
                let mut e = i + 1;
                while e < end && (is_ident(b[e]) || b"-:.@#[]".contains(&b[e])) {
                    e += 1;
                }
                self.tok(i, e, Kind::Keyword);
                let mut j = e;
                while j < end && b[j] == b' ' {
                    j += 1;
                }
                if j < end && b[j] == b'=' {
                    j += 1;
                    while j < end && b[j] == b' ' {
                        j += 1;
                    }
                    if j < end && (b[j] == b'"' || b[j] == b'\'') {
                        let q = b[j];
                        match find(b, j + 1, end, &[q]) {
                            Some(close) => {
                                self.tok(j, j + 1, Kind::Str);
                                self.embedded_expr(j + 1, close);
                                self.tok(close, close + 1, Kind::Str);
                                i = close + 1;
                            }
                            None => {
                                self.tok(j, end, Kind::Str);
                                i = end;
                            }
                        }
                        continue;
                    }
                }
                i = e;
                continue;
            }
            if c == b'>' {
                let s = if i > 0 && b[i - 1] == b'/' { i - 1 } else { i };
                self.tok(s, i + 1, Kind::Tag);
                return Some(i + 1);
            }
            if c == b'"' || c == b'\'' {
                let e = find(b, i + 1, end, &[c]).map_or(end, |p| p + 1);
                self.tok(i, e, Kind::Str);
                i = e;
            } else if is_ident_start(c) {
                let mut e = i;
                while e < end && (is_ident(b[e]) || b"-:.@".contains(&b[e])) {
                    e += 1;
                }
                self.tok(i, e, Kind::Attr);
                i = e;
            } else {
                i += 1;
            }
        }
        None
    }

    fn embedded_js(&mut self, js: u8, i: usize, end: usize) -> Result<usize, State> {
        let stop = find_ci(self.b, i, end, b"</script").unwrap_or(end);
        // 0-2: JavaScript, 3-5: TypeScript.
        let (base, lang) = if js >= 3 { (3, Lang::TypeScript) } else { (0, Lang::JavaScript) };
        let st = match js - base {
            1 => State::Comment,
            2 => State::Str(b'`'),
            _ => State::Normal,
        };
        let (saved_lang, saved_words) = (self.lang, self.words);
        self.lang = lang;
        self.words = word_table(lang);
        let (st, _) = self.code(lang.spec(), st, i, stop);
        self.lang = saved_lang;
        self.words = saved_words;
        if stop < end {
            return Ok(stop);
        }
        Err(State::HtmlScript(
            base + match st {
                State::Comment => 1,
                State::Str(_) => 2,
                _ => 0,
            },
        ))
    }

    fn embedded_css(&mut self, css: u8, i: usize, end: usize) -> Result<usize, State> {
        let stop = find_ci(self.b, i, end, b"</style").unwrap_or(end);
        let st = self.css(css, i, stop);
        if stop < end { Ok(stop) } else { Err(State::HtmlStyle(st)) }
    }

    // ----------------------------------------------------------------- CSS

    fn css(&mut self, st: u8, start: usize, end: usize) -> u8 {
        let b = self.b;
        let mut depth = st & 0x7F;
        let mut i = start;
        if st & 0x80 != 0 {
            match find(b, i, end, b"*/") {
                Some(p) => {
                    self.tok(i, p + 2, Kind::Comment);
                    i = p + 2;
                }
                None => {
                    self.tok(i, end, Kind::Comment);
                    return st;
                }
            }
        }
        while i < end {
            let c = b[i];
            match c {
                b'/' if self.at(i + 1) == b'*' => match find(b, i + 2, end, b"*/") {
                    Some(p) => {
                        self.tok(i, p + 2, Kind::Comment);
                        i = p + 2;
                    }
                    None => {
                        self.tok(i, end, Kind::Comment);
                        return depth | 0x80;
                    }
                },
                b'/' if self.at(i + 1) == b'/' => {
                    // SCSS / LESS line comment
                    self.tok(i, end, Kind::Comment);
                    return depth;
                }
                b'"' | b'\'' => {
                    let e = find(b, i + 1, end, &[c]).map_or(end, |p| p + 1);
                    self.tok(i, e, Kind::Str);
                    i = e;
                }
                b'{' => {
                    depth = (depth + 1).min(0x7F);
                    i += 1;
                }
                b'}' => {
                    depth = depth.saturating_sub(1);
                    i += 1;
                }
                b'@' => {
                    let e = self.css_ident_end(i + 1, end);
                    self.tok(i, e, Kind::Keyword);
                    i = e;
                }
                b'!' if self.starts(i, end, "!important") => {
                    self.tok(i, i + 10, Kind::Keyword);
                    i += 10;
                }
                b'#' if depth > 0 && self.at(i + 1).is_ascii_hexdigit() => {
                    let mut e = i + 1;
                    while e < end && b[e].is_ascii_alphanumeric() {
                        e += 1;
                    }
                    self.tok(i, e, Kind::Number);
                    i = e;
                }
                b'#' | b'.' if is_ident_start(self.at(i + 1)) || self.at(i + 1) == b'-' => {
                    let e = self.css_ident_end(i + 1, end);
                    self.tok(i, e, Kind::Attr);
                    i = e;
                }
                b':' if depth == 0 => {
                    let mut s = i + 1;
                    if self.at(s) == b':' {
                        s += 1;
                    }
                    let e = self.css_ident_end(s, end);
                    self.tok(i, e, Kind::Keyword);
                    i = e.max(i + 1);
                }
                _ if c.is_ascii_digit() || (c == b'.' && self.at(i + 1).is_ascii_digit()) => {
                    let mut e = i;
                    while e < end && (b[e].is_ascii_digit() || b[e] == b'.') {
                        e += 1;
                    }
                    while e < end && (b[e].is_ascii_alphabetic() || b[e] == b'%') {
                        e += 1;
                    }
                    self.tok(i, e, Kind::Number);
                    i = e;
                }
                _ if is_ident_start(c) || (c == b'-' && (is_ident_start(self.at(i + 1)) || self.at(i + 1) == b'-')) => {
                    let e = self.css_ident_end(i, end);
                    let mut j = e;
                    while j < end && b[j] == b' ' {
                        j += 1;
                    }
                    let kind = if depth == 0 {
                        Kind::Tag
                    } else if self.at(j) == b':' {
                        Kind::Attr
                    } else if self.at(e) == b'(' {
                        Kind::Function
                    } else if self.at(j) == b'{' || self.at(j) == b',' {
                        Kind::Tag
                    } else {
                        Kind::Constant
                    };
                    self.tok(i, e, kind);
                    i = e;
                }
                _ => i += 1,
            }
        }
        depth
    }

    fn css_ident_end(&self, mut i: usize, end: usize) -> usize {
        while i < end && (is_ident(self.b[i]) || self.b[i] == b'-') {
            i += 1;
        }
        i
    }

    // ---------------------------------------------------------------- YAML

    fn yaml(&mut self, state: State) -> State {
        let b = self.b;
        let end = b.len();
        let indent = b.iter().take_while(|&&c| c == b' ').count();
        if let State::YamlBlock(p) = state
            && (indent == end || indent > p as usize) {
                self.tok(indent, end, Kind::Str);
                return state;
            }
        if b.starts_with(b"---") || b.starts_with(b"...") {
            self.tok(0, 3, Kind::Preproc);
            self.yaml_value(3, end, indent);
            return State::Normal;
        }
        let mut i = indent;
        // List item markers.
        while i < end && b[i] == b'-' && (i + 1 == end || b[i + 1] == b' ') {
            i += 1;
            while i < end && b[i] == b' ' {
                i += 1;
            }
        }
        if i < end && b[i] == b'#' {
            self.tok(i, end, Kind::Comment);
            return State::Normal;
        }
        // Key?
        let key_end = if i < end && (b[i] == b'"' || b[i] == b'\'') {
            let q = b[i];
            find(b, i + 1, end, &[q]).map(|p| p + 1).filter(|&p| self.at(p) == b':')
        } else {
            let mut j = i;
            let mut found = None;
            while j < end {
                if b[j] == b':' && (j + 1 == end || b[j + 1] == b' ' || b[j + 1] == b'\t') {
                    found = Some(j);
                    break;
                }
                if b[j] == b'#' && j > i && b[j - 1] == b' ' {
                    break;
                }
                if (b[j] == b'"' || b[j] == b'\'' || b[j] == b'{' || b[j] == b'[') && j == i {
                    break;
                }
                j += 1;
            }
            found
        };
        if let Some(ke) = key_end {
            self.tok(i, ke, Kind::Attr);
            i = ke + 1;
        }
        self.yaml_value(i, end, indent)
    }

    fn yaml_value(&mut self, mut i: usize, end: usize, indent: usize) -> State {
        let b = self.b;
        while i < end && b[i] == b' ' {
            i += 1;
        }
        if i < end && (b[i] == b'|' || b[i] == b'>') {
            let mut e = i + 1;
            while e < end && b"+-0123456789".contains(&b[e]) {
                e += 1;
            }
            self.tok(i, e, Kind::Preproc);
            if let Some(p) = find(b, e, end, b"#") {
                self.tok(p, end, Kind::Comment);
            }
            return State::YamlBlock(indent.min(255) as u8);
        }
        while i < end {
            let c = b[i];
            match c {
                b'#' if i == 0 || b[i - 1] == b' ' => {
                    self.tok(i, end, Kind::Comment);
                    break;
                }
                b'"' | b'\'' => {
                    let mut j = i + 1;
                    while j < end && b[j] != c {
                        j += if b[j] == b'\\' && c == b'"' { 2 } else { 1 };
                    }
                    let e = (j + 1).min(end);
                    self.tok(i, e, Kind::Str);
                    i = e;
                }
                b'&' | b'*' if is_ident(self.at(i + 1)) => {
                    let e = self.yaml_scalar_end(i + 1, end);
                    self.tok(i, e, Kind::Variable);
                    i = e;
                }
                b'!' => {
                    let e = self.yaml_scalar_end(i + 1, end);
                    self.tok(i, e, Kind::Type);
                    i = e;
                }
                b' ' | b'[' | b']' | b'{' | b'}' | b',' => i += 1,
                _ => {
                    let e = self.yaml_scalar_end(i, end);
                    let w = &b[i..e];
                    let wl = w.strip_suffix(b":").unwrap_or(w);
                    let kind = if w.ends_with(b":") {
                        Some(Kind::Attr)
                    } else if matches!(
                        wl.to_ascii_lowercase().as_slice(),
                        b"true" | b"false" | b"yes" | b"no" | b"on" | b"off" | b"null" | b"~"
                    ) {
                        Some(Kind::Constant)
                    } else if !wl.is_empty() && (wl[0].is_ascii_digit() || (wl[0] == b'-' && wl.len() > 1 && wl[1].is_ascii_digit()) || wl[0] == b'.')
                        && wl.iter().all(|c| c.is_ascii_hexdigit() || b".-+_xXoO".contains(c))
                    {
                        Some(Kind::Number)
                    } else {
                        None
                    };
                    if let Some(k) = kind {
                        self.tok(i, e, k);
                    }
                    i = e.max(i + 1);
                }
            }
        }
        State::Normal
    }

    fn yaml_scalar_end(&self, mut i: usize, end: usize) -> usize {
        while i < end && !b" ,[]{}".contains(&self.b[i]) {
            i += 1;
        }
        i
    }

    // ----------------------------------------------------------------- PHP

    fn php(&mut self, mut state: State) -> State {
        let b = self.b;
        let end = b.len();
        let mut i = 0;
        loop {
            if state == State::PhpHtml {
                let open = find(b, i, end, b"<?").unwrap_or(end);
                self.html(State::Normal, i, open);
                if open == end {
                    return State::PhpHtml;
                }
                i = open;
                state = State::Normal;
            }
            let (st, stop) = self.code(Lang::Php.spec(), state, i, end);
            if st != State::PhpHtml || stop >= end {
                return st;
            }
            i = stop;
            state = State::PhpHtml;
        }
    }
}

impl Lexer<'_> {
    // ------------------------------------------------------------ Markdown

    fn markdown(&mut self, state: State, line: &str) -> State {
        let b = self.b;
        let end = b.len();
        let indent = b.iter().take_while(|&&c| c == b' ' || c == b'\t').count();
        let t = &b[indent..];
        let run = |c: u8| t.iter().take_while(|&&x| x == c).count();

        if let State::MdFence(li, fc, inner) = state {
            let n = run(fc);
            if indent <= 3 && n >= 3 && t[n..].iter().all(|c| c.is_ascii_whitespace()) {
                self.tok(0, end, Kind::Preproc);
                return State::Normal;
            }
            let lang = crate::lang::ALL[li as usize];
            if lang == Lang::Plain {
                self.tok(0, end, Kind::Str);
                return state;
            }
            let mut inner_toks = Vec::new();
            let st = lex_line(lang, unpack(inner), line, &mut inner_toks);
            self.out.extend(inner_toks);
            return State::MdFence(li, fc, pack(st));
        }

        if indent <= 3 && (t.starts_with(b"```") || t.starts_with(b"~~~")) {
            let fc = t[0];
            let n = run(fc);
            self.tok(0, indent + n, Kind::Preproc);
            let info = line[indent + n..].trim();
            if !info.is_empty() {
                self.tok(indent + n, end, Kind::Type);
            }
            let lang = Lang::from_fence(info);
            let li = crate::lang::ALL.iter().position(|&l| l == lang).unwrap_or(0) as u8;
            return State::MdFence(li, fc, pack(initial_state(lang)));
        }

        if indent <= 3 && !t.is_empty() {
            // ATX heading.
            let h = run(b'#');
            if (1..=6).contains(&h) && (t.len() == h || t[h] == b' ' || t[h] == b'\t') {
                self.tok(0, end, Kind::Heading);
                return State::Normal;
            }
            // Setext underline / thematic break.
            let first = t[0];
            let body: Vec<u8> = t.iter().copied().filter(|c| !c.is_ascii_whitespace()).collect();
            if body.len() >= 3 && body.iter().all(|&c| c == first) {
                match first {
                    b'=' => {
                        self.tok(0, end, Kind::Heading);
                        return State::Normal;
                    }
                    b'-' | b'*' | b'_' => {
                        self.tok(0, end, Kind::Preproc);
                        return State::Normal;
                    }
                    _ => {}
                }
            }
            // Block quote.
            if first == b'>' {
                self.tok(indent, end, Kind::Comment);
                return State::Normal;
            }
        }

        // List item marker and task box.
        let mut i = indent;
        let digits = t.iter().take_while(|c| c.is_ascii_digit()).count();
        let marker = if !t.is_empty() && b"-*+".contains(&t[0]) && (t.len() == 1 || t[1] == b' ') {
            1
        } else if (1..=9).contains(&digits)
            && t.len() > digits
            && (t[digits] == b'.' || t[digits] == b')')
            && (t.len() == digits + 1 || t[digits + 1] == b' ')
        {
            digits + 1
        } else {
            0
        };
        if marker > 0 {
            self.tok(i, i + marker, Kind::Variable);
            i += marker;
            while i < end && b[i] == b' ' {
                i += 1;
            }
            if end - i >= 3 && b[i] == b'[' && b[i + 2] == b']' && b" xX".contains(&b[i + 1]) {
                self.tok(i, i + 3, Kind::Constant);
                i += 3;
            }
        }
        self.md_inline(i, end);
        State::Normal
    }

    fn md_inline(&mut self, mut i: usize, end: usize) {
        let b = self.b;
        while i < end {
            let c = b[i];
            match c {
                b'\\' => i += 2,
                b'`' => {
                    let n = b[i..end].iter().take_while(|&&x| x == b'`').count();
                    let mut j = i + n;
                    let mut close = None;
                    while let Some(p) = find(b, j, end, b"`") {
                        let m = b[p..end].iter().take_while(|&&x| x == b'`').count();
                        if m == n {
                            close = Some(p + m);
                            break;
                        }
                        j = p + m;
                    }
                    match close {
                        Some(e) => {
                            self.tok(i, e, Kind::Str);
                            i = e;
                        }
                        None => i += n,
                    }
                }
                b'<' => {
                    let gt = find(b, i + 1, end, b">");
                    match gt {
                        Some(g) if !b[i + 1..g].contains(&b' ') && b[i + 1..g].iter().any(|&x| x == b':' || x == b'@') => {
                            self.tok(i, g + 1, Kind::Link);
                            i = g + 1;
                        }
                        Some(g) if is_ident_start(self.at(i + 1)) || self.at(i + 1) == b'/' || self.at(i + 1) == b'!' => {
                            let kind = if self.at(i + 1) == b'!' { Kind::Comment } else { Kind::Tag };
                            self.tok(i, g + 1, kind);
                            i = g + 1;
                        }
                        _ => i += 1,
                    }
                }
                b'[' | b'!' if c == b'[' || self.at(i + 1) == b'[' => {
                    let open = if c == b'!' { i + 1 } else { i };
                    // Matching ']' (nested brackets allowed).
                    let mut depth = 0;
                    let mut close = None;
                    for (k, &x) in b[open..end].iter().enumerate() {
                        match x {
                            b'[' => depth += 1,
                            b']' => {
                                depth -= 1;
                                if depth == 0 {
                                    close = Some(open + k);
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    let Some(rb) = close else {
                        i += 1;
                        continue;
                    };
                    match self.at(rb + 1) {
                        b'(' => {
                            let rp = find(b, rb + 2, end, b")").map_or(end, |p| p + 1);
                            self.tok(i, rb + 1, Kind::Link);
                            self.tok(rb + 1, rp, Kind::Str);
                            i = rp;
                        }
                        b'[' => {
                            let rr = find(b, rb + 2, end, b"]").map_or(end, |p| p + 1);
                            self.tok(i, rb + 1, Kind::Link);
                            self.tok(rb + 1, rr, Kind::Variable);
                            i = rr;
                        }
                        b':' if open == i => {
                            // Link reference definition: [id]: url
                            self.tok(i, rb + 1, Kind::Variable);
                            self.tok(rb + 2, end, Kind::Link);
                            i = end;
                        }
                        _ => i = open + 1,
                    }
                }
                b'*' | b'_' => {
                    let n = b[i..end].iter().take_while(|&&x| x == c).count();
                    let prev = if i > 0 { b[i - 1] } else { b' ' };
                    let next = self.at(i + n);
                    let opens = n <= 3 && i + n < end && !next.is_ascii_whitespace() && !(c == b'_' && prev.is_ascii_alphanumeric());
                    let close = if opens { self.md_closing(c, n, i + n, end) } else { None };
                    match close {
                        Some(e) => {
                            if n >= 2 {
                                self.tok(i, e, Kind::Strong);
                            }
                            if n != 2 {
                                self.tok(i, e, Kind::Emph);
                            }
                            i = e;
                        }
                        None => i += n,
                    }
                }
                b'~' if self.at(i + 1) == b'~' => match find(b, i + 2, end, b"~~") {
                    Some(p) => {
                        self.tok(i, p + 2, Kind::Comment);
                        i = p + 2;
                    }
                    None => i += 2,
                },
                b'h' if (self.starts(i, end, "http://") || self.starts(i, end, "https://"))
                    && (i == 0 || !is_ident(b[i - 1])) =>
                {
                    let mut e = i;
                    while e < end && !b[e].is_ascii_whitespace() && b[e] != b'>' && b[e] != b')' {
                        e += 1;
                    }
                    self.tok(i, e, Kind::Link);
                    i = e;
                }
                _ => i += 1,
            }
        }
    }

    /// End of an emphasis run of `n` × `c` that started before `from`.
    fn md_closing(&self, c: u8, n: usize, from: usize, end: usize) -> Option<usize> {
        let b = self.b;
        let mut j = from;
        while j < end {
            if b[j] == b'\\' {
                j += 2;
                continue;
            }
            if b[j] == b'`' {
                // Code spans hide delimiters.
                let m = b[j..end].iter().take_while(|&&x| x == b'`').count();
                j = find(b, j + m, end, &b[j..j + m]).map_or(j + m, |p| p + m);
                continue;
            }
            if b[j] == c {
                let m = b[j..end].iter().take_while(|&&x| x == c).count();
                let after = b.get(j + m).copied().unwrap_or(b' ');
                if m == n && !b[j - 1].is_ascii_whitespace() && !(c == b'_' && after.is_ascii_alphanumeric()) {
                    return Some(j + m);
                }
                j += m;
                continue;
            }
            j += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lang: Lang, st: State, line: &str) -> (Vec<(String, Kind)>, State) {
        let mut v = Vec::new();
        let s = lex_line(lang, st, line, &mut v);
        (
            v.iter().map(|t| (line[t.start as usize..t.end as usize].to_string(), t.kind)).collect(),
            s,
        )
    }

    #[test]
    fn rust_line() {
        let (t, s) = kinds(Lang::Rust, State::Normal, r#"pub fn x<'a>(v: &'a str) -> u32 { println!("hi {}", 'c'); 0x1F } // c"#);
        assert_eq!(s, State::Normal);
        assert!(t.contains(&("pub".into(), Kind::Keyword)));
        assert!(t.contains(&("x".into(), Kind::Function)));
        assert!(t.contains(&("'a".into(), Kind::Variable)));
        assert!(t.contains(&("u32".into(), Kind::Type)));
        assert!(t.contains(&("println!".into(), Kind::Preproc)));
        assert!(t.contains(&("\"hi {}\"".into(), Kind::Str)));
        assert!(t.contains(&("'c'".into(), Kind::Str)));
        assert!(t.contains(&("0x1F".into(), Kind::Number)));
        assert!(t.contains(&("// c".into(), Kind::Comment)));
    }

    #[test]
    fn multiline_states() {
        let (_, s) = kinds(Lang::C, State::Normal, "int a; /* start");
        assert_eq!(s, State::Comment);
        let (t, s) = kinds(Lang::C, s, "end */ int b;");
        assert_eq!(s, State::Normal);
        assert_eq!(t[0], ("end */".into(), Kind::Comment));
        let (_, s) = kinds(Lang::Python, State::Normal, "x = '''doc");
        assert_eq!(s, State::Triple(b'\''));
        let (_, s) = kinds(Lang::Rust, State::Normal, r###"let s = r#"raw"###);
        assert_eq!(s, State::RawStr(1));
        let (_, s) = kinds(Lang::Rust, s, r###"still"# ;"###);
        assert_eq!(s, State::Normal);
    }

    #[test]
    fn shell() {
        let (t, _) = kinds(Lang::Bash, State::Normal, r#"echo "$HOME ${x}" # c  $#"#);
        assert!(t.contains(&("$HOME".into(), Kind::Variable)));
        assert!(t.contains(&("${x}".into(), Kind::Variable)));
        assert!(t.contains(&("# c  $#".into(), Kind::Comment)));
        let (t, _) = kinds(Lang::Bash, State::Normal, "n=${#a}");
        assert!(!t.iter().any(|x| x.1 == Kind::Comment));
    }

    #[test]
    fn sql() {
        let (t, s) = kinds(Lang::Sql, State::Normal, "SELECT count(*), Name::TEXT FROM t WHERE x IS NOT NULL AND y = 'it''s' -- c");
        assert_eq!(s, State::Normal);
        assert!(t.contains(&("SELECT".into(), Kind::Keyword)));
        assert!(t.contains(&("count".into(), Kind::Function)));
        assert!(t.contains(&("TEXT".into(), Kind::Type)));
        assert!(t.contains(&("NULL".into(), Kind::Constant)));
        assert!(!t.iter().any(|x| x.0 == "Name"));
        assert!(t.contains(&("-- c".into(), Kind::Comment)));
        let (_, s) = kinds(Lang::Sql, State::Normal, "INSERT INTO t VALUES ('multi");
        assert_eq!(s, State::Str(b'\''));
        let (t, s) = kinds(Lang::Sql, s, "line'); /* x");
        assert_eq!(t[0], ("line'".into(), Kind::Str));
        assert_eq!(s, State::Comment);
    }

    #[test]
    fn vue() {
        let (t, s) = kinds(
            Lang::Vue,
            State::Normal,
            r#"<div v-if="ok && n > 1" :class="cls" @click="go(1)" #item>{{ msg.toUpperCase() }}</div>"#,
        );
        assert_eq!(s, State::Normal);
        for d in ["v-if", ":class", "@click", "#item"] {
            assert!(t.contains(&(d.into(), Kind::Keyword)), "{d}");
        }
        assert!(t.contains(&("go".into(), Kind::Function)));
        assert!(t.contains(&("1".into(), Kind::Number)));
        assert!(t.contains(&("{{".into(), Kind::Preproc)));
        assert!(t.contains(&("toUpperCase".into(), Kind::Function)));
        assert!(!t.iter().any(|x| x.0.contains("ok &&")));
        let (_, s) = kinds(Lang::Vue, State::Normal, r#"<script setup lang="ts">"#);
        assert_eq!(s, State::HtmlScript(3));
        let (t, s) = kinds(Lang::Vue, s, "interface Props { n: number } // nota");
        assert_eq!(s, State::HtmlScript(3));
        assert!(t.contains(&("interface".into(), Kind::Keyword)));
        assert!(t.contains(&("number".into(), Kind::Type)));
        let (_, s) = kinds(Lang::Vue, s, "</script>");
        assert_eq!(s, State::Normal);
        // Plain HTML: no directives.
        let (t, _) = kinds(Lang::Html, State::Normal, r#"<p :x="a">{{ b }}</p>"#);
        assert!(!t.iter().any(|x| x.1 == Kind::Keyword));
    }

    #[test]
    fn go_receiver() {
        let (t, _) = kinds(Lang::Go, State::Normal, "func (r *T) Name() error {");
        assert!(!t.contains(&("r".into(), Kind::Function)));
        assert!(t.contains(&("Name".into(), Kind::Function)));
    }

    #[test]
    fn html_and_php() {
        let (t, s) = kinds(Lang::Html, State::Normal, r#"<a href="x">&amp;</a><script>let x = 1;"#);
        assert_eq!(s, State::HtmlScript(0));
        assert!(t.contains(&("href".into(), Kind::Attr)));
        assert!(t.contains(&("let".into(), Kind::Keyword)));
        let (t, s) = kinds(Lang::Php, State::PhpHtml, r#"<b><?php echo $x; ?></b>"#);
        assert_eq!(s, State::PhpHtml);
        assert!(t.contains(&("$x".into(), Kind::Variable)));
        assert!(t.contains(&("<b".into(), Kind::Tag)));
        assert!(t.contains(&("</b".into(), Kind::Tag)));
        let (_, s) = kinds(Lang::Php, State::PhpHtml, "<?php");
        assert_eq!(s, State::Normal);
    }

    #[test]
    fn markdown() {
        let k = |st, l| kinds(Lang::Markdown, st, l);
        assert_eq!(k(State::Normal, "## Title").0, vec![("## Title".into(), Kind::Heading)]);
        let (t, _) = k(State::Normal, "- [x] **bold** and *it* `code` [link](http://x) snake_case_word");
        assert!(t.contains(&("-".into(), Kind::Variable)));
        assert!(t.contains(&("[x]".into(), Kind::Constant)));
        assert!(t.contains(&("**bold**".into(), Kind::Strong)));
        assert!(t.contains(&("*it*".into(), Kind::Emph)));
        assert!(t.contains(&("`code`".into(), Kind::Str)));
        assert!(t.contains(&("[link]".into(), Kind::Link)));
        assert!(!t.iter().any(|x| x.0.contains("_case_")));
        // Fenced code keeps the inner language state across lines.
        let (t, s) = k(State::Normal, "```python");
        assert!(t.contains(&("python".into(), Kind::Type)));
        let (_, s) = k(s, "x = '''doc");
        assert!(matches!(s, State::MdFence(_, b'`', _)));
        let (t, s) = k(s, "still doc''' ; import os");
        assert!(t.contains(&("still doc'''".into(), Kind::Str)));
        assert!(t.contains(&("import".into(), Kind::Keyword)));
        let (t, s) = k(s, "```");
        assert_eq!((t[0].1, s), (Kind::Preproc, State::Normal));
        for st in [State::Comment, State::Css(0x85), State::HtmlStyle(0x81), State::YamlBlock(4), State::RawStr(2)] {
            assert_eq!(unpack(pack(st)), st);
        }
    }

    #[test]
    fn yaml_css() {
        let (t, s) = kinds(Lang::Yaml, State::Normal, "  key: |");
        assert_eq!(t[0], ("key".into(), Kind::Attr));
        assert_eq!(s, State::YamlBlock(2));
        let (t, _) = kinds(Lang::Yaml, State::Normal, "- name: true # c");
        assert!(t.contains(&("true".into(), Kind::Constant)));
        let (t, s) = kinds(Lang::Css, State::Css(0), "a.b:hover { color: #fff; margin: 1px }");
        assert_eq!(s, State::Css(0));
        assert!(t.contains(&("color".into(), Kind::Attr)));
        assert!(t.contains(&("#fff".into(), Kind::Number)));
        assert!(t.contains(&("1px".into(), Kind::Number)));
    }
}
