//! Source formatting.
//!
//! When a well known formatter is installed (rustfmt, clang-format, gofmt,
//! black/ruff, shfmt, prettier, pg_format) it is used; otherwise a built-in
//! re-indenter based on the lexer is applied.

use crate::lang::Lang;
use crate::lexer::{self, Kind, State, Token};
use std::path::{Path, PathBuf};

pub struct Formatted {
    pub text: String,
    pub tool: String,
}

fn has_clang_format_config(path: Option<&Path>) -> bool {
    let mut dir = path.and_then(|p| p.parent());
    while let Some(d) = dir {
        if d.join(".clang-format").exists() || d.join("_clang-format").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

/// External formatter candidates for a language: (command, args).
fn external(lang: Lang, path: Option<&Path>, tab: u32) -> Vec<(&'static str, Vec<String>)> {
    let ext = match lang {
        Lang::C => "c",
        Lang::Cpp => "cpp",
        Lang::Java => "java",
        Lang::JavaScript => "js",
        Lang::TypeScript => "ts",
        Lang::Tsx => "tsx",
        Lang::CSharp => "cs",
        Lang::Css => "css",
        Lang::Html => "html",
        Lang::Vue => "vue",
        Lang::Yaml => "yaml",
        Lang::Markdown => "md",
        _ => "txt",
    };
    let fake = path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("untitled.{ext}"));
    let clang = || {
        let style = if has_clang_format_config(path) {
            "-style=file".to_string()
        } else {
            format!("-style={{BasedOnStyle: LLVM, IndentWidth: {tab}, ColumnLimit: 100}}")
        };
        ("clang-format", vec![format!("--assume-filename={fake}"), style])
    };
    let prettier = || ("prettier", vec!["--stdin-filepath".to_string(), fake.clone()]);
    match lang {
        Lang::Rust => vec![("rustfmt", vec!["--edition".into(), "2024".into()])],
        Lang::C | Lang::Cpp | Lang::Java | Lang::CSharp => vec![clang()],
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => vec![prettier(), clang()],
        Lang::Perl => vec![("perltidy", vec!["-st".into(), "-se".into(), format!("-i={tab}")])],
        Lang::Css | Lang::Html | Lang::Vue | Lang::Yaml | Lang::Markdown => vec![prettier()],
        Lang::Go => vec![("gofmt", vec![])],
        Lang::Python => vec![("ruff", vec!["format".into(), "-".into()]), ("black", vec!["-q".into(), "-".into()])],
        Lang::Bash => vec![("shfmt", vec!["-i".into(), tab.to_string()])],
        Lang::Sql => vec![("pg_format", vec!["-s".into(), tab.to_string(), "-".into()])],
        _ => vec![],
    }
}

/// An installed external formatter for `lang`: (name, program, args).
/// The text goes to its standard input, the result comes from its output.
pub fn external_formatter(lang: Lang, path: Option<&Path>, tab: u32) -> Option<(&'static str, PathBuf, Vec<String>)> {
    external(lang, path, tab).into_iter().find_map(|(cmd, args)| crate::proc::find_in_path(cmd).map(|exe| (cmd, exe, args)))
}

/// Format `text` with the built-in formatter (used when no external one
/// is installed).
pub fn format_internal(lang: Lang, text: &str, tab: u32, spaces: bool) -> Formatted {
    let unit = if spaces && !lang.prefers_tabs() { " ".repeat(tab as usize) } else { "\t".to_string() };
    let text = match lang {
        Lang::C | Lang::Cpp | Lang::CSharp | Lang::Rust | Lang::Java | Lang::JavaScript | Lang::TypeScript
        | Lang::Tsx | Lang::Go | Lang::Php | Lang::Css | Lang::Perl => reindent_braces(lang, text, &unit),
        Lang::Bash => reindent_shell(text, &unit),
        Lang::Html | Lang::Vue => reindent_html(text, &unit),
        Lang::Markdown => tidy_markdown(text),
        _ => tidy(text, (lang == Lang::Python || lang == Lang::Yaml).then_some(tab)),
    };
    Formatted { text, tool: "formattatore interno".into() }
}

/// Trim trailing whitespace, expand leading tabs (if `tab` is set) and end
/// the text with exactly one newline.
pub fn tidy(text: &str, tab: Option<u32>) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let line = line.trim_end();
        match tab {
            Some(t) if line.starts_with('\t') => {
                let ws = line.len() - line.trim_start_matches('\t').len();
                out.push_str(&" ".repeat(ws * t as usize));
                out.push_str(&line[ws..]);
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Conservative Markdown clean-up: trailing whitespace is removed except
/// for hard line breaks (two or more spaces, kept as exactly two), runs of
/// blank lines are collapsed, fenced code blocks are left untouched.
pub fn tidy_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    let mut blank_run = 0;
    for line in text.lines() {
        let t = line.trim_start();
        let fence_of = |t: &str| {
            let c = t.chars().next()?;
            let n = t.chars().take_while(|&x| x == c).count();
            ((c == '`' || c == '~') && n >= 3).then_some((c, n))
        };
        if let Some((c, n)) = fence {
            if fence_of(t).is_some_and(|(c2, n2)| c2 == c && n2 >= n) && t.trim_end().chars().all(|x| x == c) {
                fence = None;
            }
            out.push_str(if fence.is_none() { line.trim_end() } else { line });
            out.push('\n');
            continue;
        }
        if line.len() - line.trim_start().len() <= 3 {
            fence = fence_of(t);
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(trimmed);
        if fence.is_none() && !trimmed.is_empty() && !t.starts_with('#') && line[trimmed.len()..].starts_with("  ") {
            out.push_str("  ");
        }
        out.push('\n');
    }
    finish(out)
}

/// Markdown list/quote continuation for the text before the cursor.
/// Returns the prefix for the next line and whether the current item is
/// empty (in which case pressing Enter ends the list instead).
pub fn md_list_continuation(prefix: &str) -> Option<(String, bool)> {
    let indent_len = prefix.len() - prefix.trim_start_matches([' ', '\t']).len();
    let (indent, rest) = prefix.split_at(indent_len);
    // Block quote markers ("> > ").
    let quote_len = rest.len() - rest.trim_start_matches(['>', ' ']).len();
    let (quote, rest) = rest.split_at(quote_len);
    let quote = if quote.contains('>') { quote } else { "" };
    let rest = if quote.is_empty() { prefix[indent_len..].as_ref() } else { rest };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (marker, body) = if let Some(b) = ["- ", "* ", "+ "].iter().find_map(|m| rest.strip_prefix(m).map(|b| (m.to_string(), b))) {
        b
    } else if (1..=9).contains(&digits) && (rest[digits..].starts_with(". ") || rest[digits..].starts_with(") ")) {
        let n: u64 = rest[..digits].parse().ok()?;
        let sep = &rest[digits..digits + 1];
        (format!("{}{sep} ", n + 1), &rest[digits + 2..])
    } else if !quote.is_empty() {
        (String::new(), rest)
    } else {
        return None;
    };
    // Task lists continue with an unchecked box.
    let (task, body) = match body.get(..4) {
        Some("[ ] " | "[x] " | "[X] ") => ("[ ] ", &body[4..]),
        _ => ("", body),
    };
    let empty = body.trim().is_empty();
    Some((format!("{indent}{quote}{marker}{task}"), empty))
}

/// Byte ranges of string and comment tokens.
fn masked(toks: &[Token]) -> Vec<(usize, usize)> {
    toks.iter()
        .filter(|t| matches!(t.kind, Kind::Str | Kind::Comment))
        .map(|t| (t.start as usize, t.end as usize))
        .collect()
}

fn in_mask(mask: &[(usize, usize)], pos: usize) -> bool {
    mask.iter().any(|&(s, e)| pos >= s && pos < e)
}

/// Re-indent brace delimited languages by nesting depth, skipping strings
/// and comments (as reported by the lexer).
pub fn reindent_braces(lang: Lang, text: &str, unit: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut depth: i32 = 0;
    let mut state = lexer::initial_state(lang);
    let mut toks = Vec::new();
    for line in text.lines() {
        let start_state = state;
        state = lexer::lex_line(lang, state, line, &mut toks);
        let trimmed = line.trim();
        let off = line.len() - line.trim_start().len();
        let mask = masked(&toks);
        let verbatim = matches!(start_state, State::Str(_) | State::Triple(_) | State::RawStr(_) | State::PhpHtml);
        let mut lead_close = 0;
        let mut net = 0;
        let mut leading = true;
        for (i, c) in line.bytes().enumerate().skip(off) {
            if in_mask(&mask, i) {
                leading = false;
                continue;
            }
            match c {
                b'{' | b'[' | b'(' => {
                    net += 1;
                    leading = false;
                }
                b'}' | b']' | b')' => {
                    net -= 1;
                    if leading {
                        lead_close += 1;
                    }
                }
                b' ' | b'\t' => {}
                _ => leading = false,
            }
        }
        if verbatim {
            out.push_str(line.trim_end());
        } else if !trimmed.is_empty() {
            let level = (depth - lead_close).max(0) as usize;
            for _ in 0..level {
                out.push_str(unit);
            }
            if start_state == State::Comment && trimmed.starts_with('*') {
                out.push(' ');
            }
            out.push_str(trimmed);
        }
        out.push('\n');
        depth = (depth + net).max(0);
    }
    finish(out)
}

fn finish(mut out: String) -> String {
    let n = out.trim_end_matches('\n').len();
    out.truncate(n);
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Re-indent shell scripts by `if/then/fi`, `do/done`, `case/esac`, `{}`.
pub fn reindent_shell(text: &str, unit: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth: i32 = 0;
    let mut state = State::Normal;
    let mut toks = Vec::new();
    for line in text.lines() {
        let start_state = state;
        state = lexer::lex_line(Lang::Bash, state, line, &mut toks);
        if matches!(start_state, State::Str(_)) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let mask = masked(&toks);
        // Code with strings/comments blanked out.
        let code: String = line
            .char_indices()
            .map(|(i, c)| if in_mask(&mask, i) { ' ' } else { c })
            .collect();
        let words: Vec<&str> = code.split(|c: char| c.is_whitespace() || c == ';').filter(|w| !w.is_empty()).collect();
        let first = words.first().copied().unwrap_or("");
        let trimmed = line.trim();
        let mut level = depth;
        let mut net = 0;
        match first {
            "fi" | "done" | "esac" | "}" => level -= 1,
            "else" | "elif" => level -= 1,
            _ => {}
        }
        for w in &words {
            match *w {
                "then" if first != "elif" => net += 1,
                "do" | "case" | "{" => net += 1,
                "fi" | "done" | "esac" | "}" => net -= 1,
                _ => {}
            }
        }
        let code_t = code.trim_end();
        let is_pattern = first != "case" && code_t.ends_with(')') && !code_t.contains('(') && !code_t.contains("$(");
        if is_pattern {
            net += 1;
        }
        if code_t.ends_with(";;") || code_t.ends_with(";&") {
            net -= 1;
        }
        if !trimmed.is_empty() {
            for _ in 0..level.max(0) {
                out.push_str(unit);
            }
            out.push_str(trimmed);
        }
        out.push('\n');
        depth = (depth + net).max(0);
    }
    finish(out)
}

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "!doctype",
];

/// Position of `needle` in `hay` (bytes).
/// Re-indent HTML by tag nesting (and braces inside script/style).
/// The scan works on bytes: positions may fall inside multi-byte characters.
pub fn reindent_html(text: &str, unit: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth: i32 = 0;
    let mut in_comment = false;
    let mut in_code = false;
    let mut in_pre = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let b = trimmed.as_bytes();
        let starts_close = trimmed.starts_with("</") || (in_code && matches!(b.first(), Some(b'}' | b']' | b')')));
        let level = depth - starts_close as i32;
        if in_pre && !trimmed.starts_with("</pre") {
            out.push_str(line);
        } else if !trimmed.is_empty() {
            for _ in 0..level.max(0) {
                out.push_str(unit);
            }
            out.push_str(trimmed);
        }
        out.push('\n');
        let mut i = 0;
        while i < b.len() {
            if in_comment {
                match lexer::find(b, i, b.len(), b"-->") {
                    Some(p) => {
                        in_comment = false;
                        i = p + 3;
                    }
                    None => break,
                }
                continue;
            }
            let c = b[i];
            if b[i..].starts_with(b"<!--") {
                in_comment = true;
                i += 4;
            } else if c == b'<' {
                let closing = b.get(i + 1) == Some(&b'/');
                let ns = i + 1 + closing as usize;
                let ne = ns + b[ns..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'-' || **c == b'!').count();
                if ne == ns {
                    i += 1;
                    continue;
                }
                let name = String::from_utf8_lossy(&b[ns..ne]).to_ascii_lowercase();
                let gt = b[ne..].iter().position(|&x| x == b'>').map(|p| p + ne);
                let self_closing = gt.is_some_and(|g| b[g - 1] == b'/');
                if closing {
                    depth -= 1;
                    if name == "script" || name == "style" {
                        in_code = false;
                    }
                    if name == "pre" {
                        in_pre = false;
                    }
                } else if !self_closing && !VOID_TAGS.contains(&name.as_str()) {
                    depth += 1;
                    if name == "script" || name == "style" {
                        in_code = true;
                    }
                    if name == "pre" {
                        in_pre = true;
                    }
                }
                i = gt.map_or(b.len(), |g| g + 1);
            } else if in_code && (c == b'"' || c == b'\'' || c == b'`') {
                i += 1 + b[i + 1..].iter().position(|&x| x == c).unwrap_or(b.len());
            } else {
                if in_code {
                    match c {
                        b'{' | b'[' | b'(' => depth += 1,
                        b'}' | b']' | b')' => depth -= 1,
                        _ => {}
                    }
                }
                i += 1;
            }
        }
        depth = depth.max(0);
    }
    finish(out)
}

/// Last position of `needle` (lowercase ASCII) in `hay`, ignoring case.
fn rfind_ignore_case(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w.eq_ignore_ascii_case(needle))
}

/// Language at the end of `before` in an HTML or Vue document: the script
/// (TypeScript with `lang="ts"`), the style, or the markup.
pub fn embedded_lang(lang: Lang, before: &str) -> Lang {
    if !matches!(lang, Lang::Html | Lang::Vue) {
        return lang;
    }
    let b = before.as_bytes();
    let inside = |open: &[u8], close: &[u8]| rfind_ignore_case(b, open).filter(|&s| rfind_ignore_case(b, close).is_none_or(|e| e < s));
    if let Some(s) = inside(b"<script", b"</script") {
        let tag = &b[s..b[s..].iter().position(|&c| c == b'>').map_or(b.len(), |e| s + e)];
        return if crate::lang::script_is_ts(tag) { Lang::TypeScript } else { Lang::JavaScript };
    }
    if inside(b"<style", b"</style").is_some() {
        return Lang::Css;
    }
    lang
}

/// Comment or uncomment the given lines (all commented -> uncomment).
pub fn toggle_comment(lang: Lang, lines: &[&str]) -> Option<Vec<String>> {
    let (pre, post) = lang.comment_tokens()?;
    Some(toggle_comment_with(pre, post, lines))
}

/// Comment or uncomment `lines` with the given prefix and suffix.
pub fn toggle_comment_with(pre: &str, post: &str, lines: &[&str]) -> Vec<String> {
    let nonempty = lines.iter().filter(|l| !l.trim().is_empty());
    let all_commented = nonempty.clone().count() > 0
        && nonempty.clone().all(|l| l.trim_start().starts_with(pre) && l.trim_end().ends_with(post));
    let min_indent = nonempty.map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                return l.to_string();
            }
            if all_commented {
                let ind = l.len() - l.trim_start().len();
                let body = &l[ind + pre.len()..];
                let body = body.strip_prefix(' ').unwrap_or(body);
                let body = if post.is_empty() {
                    body
                } else {
                    let b = body.trim_end().strip_suffix(post).unwrap_or(body);
                    b.strip_suffix(' ').unwrap_or(b)
                };
                format!("{}{}", &l[..ind], body)
            } else if post.is_empty() {
                format!("{}{} {}", &l[..min_indent], pre, &l[min_indent..])
            } else {
                format!("{}{} {} {}", &l[..min_indent], pre, &l[min_indent..], post)
            }
        })
        .collect()
}

/// Join `lines` into one: the indentation of the following lines and
/// trailing spaces become a single space, blank lines disappear. No space
/// is added inside brackets or before `,` `;` `.`, and consecutive line
/// comments become one comment.
pub fn join_lines(lang: Lang, lines: &[&str]) -> String {
    fn comment_body<'a>(comments: &[&str], t: &'a str) -> Option<&'a str> {
        comments.iter().find_map(|c| t.strip_prefix(c)).map(str::trim_start)
    }
    let comments = lang.spec().line_comment;
    let mut out = String::new();
    let mut in_comment = false;
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(line.trim_end());
            in_comment = comment_body(comments, line.trim_start()).is_some();
            continue;
        }
        let mut t = line.trim();
        if t.is_empty() {
            continue;
        }
        match comment_body(comments, t) {
            Some(body) if in_comment => t = body,
            Some(_) => in_comment = true,
            None => in_comment = false,
        }
        if t.is_empty() {
            continue;
        }
        let tight = out.trim().is_empty()
            || out.ends_with(['(', '['])
            || t.starts_with([')', ']', ',', ';', '.']);
        if !tight {
            out.push(' ');
        }
        out.push_str(t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join() {
        let j = |lang, s: &str| join_lines(lang, &s.split('\n').collect::<Vec<_>>());
        assert_eq!(j(Lang::Rust, "    let x = foo(\n        a,\n\n        b,  \n    );"), "    let x = foo(a, b,);");
        assert_eq!(j(Lang::JavaScript, "items\n  .map(f)\n  .filter(g)"), "items.map(f).filter(g)");
        assert_eq!(j(Lang::Python, "# una frase\n#   che continua\nx = 1"), "# una frase che continua x = 1");
        assert_eq!(j(Lang::Plain, "Una riga   \ne un'altra\n   e l'ultima"), "Una riga e un'altra e l'ultima");
        assert_eq!(j(Lang::C, "\n  a"), "a");
    }

    #[test]
    fn braces() {
        let src = "fn main() {\nlet s = \"{\";\n    if x {\n  y();\n      }\n}\n";
        let want = "fn main() {\n    let s = \"{\";\n    if x {\n        y();\n    }\n}\n";
        assert_eq!(reindent_braces(Lang::Rust, src, "    "), want);
        let src = "/*\n* doc\n*/\nint f(int a,\nint b) {\nreturn a;\n}";
        let want = "/*\n * doc\n */\nint f(int a,\n    int b) {\n    return a;\n}\n";
        assert_eq!(reindent_braces(Lang::C, src, "    "), want);
    }

    #[test]
    fn shell() {
        let src = "if x; then\necho a\nelse\necho b\nfi\nfor i in 1; do\ncase $i in\na)\necho;;\nesac\ndone\n";
        let want = "if x; then\n  echo a\nelse\n  echo b\nfi\nfor i in 1; do\n  case $i in\n    a)\n      echo;;\n  esac\ndone\n";
        assert_eq!(reindent_shell(src, "  "), want);
    }

    #[test]
    fn html() {
        let src = "<html>\n<body>\n<br>\n<p>x</p>\n<script>\nif (a) {\nb();\n}\n</script>\n</body>\n</html>";
        let want = "<html>\n  <body>\n    <br>\n    <p>x</p>\n    <script>\n      if (a) {\n        b();\n      }\n    </script>\n  </body>\n</html>\n";
        assert_eq!(reindent_html(src, "  "), want);
    }

    #[test]
    fn markdown() {
        let src = "# T  \n\n\n\ntext with break  \nnext\t\n```\ncode   \n\n\n```\n";
        let want = "# T\n\ntext with break  \nnext\n```\ncode   \n\n\n```\n";
        assert_eq!(tidy_markdown(src), want);
    }

    #[test]
    fn markdown_lists() {
        let c = |p: &str| md_list_continuation(p);
        assert_eq!(c("  - item"), Some(("  - ".into(), false)));
        assert_eq!(c("9. nine"), Some(("10. ".into(), false)));
        assert_eq!(c("- [x] done"), Some(("- [ ] ".into(), false)));
        assert_eq!(c("> quoted"), Some(("> ".into(), false)));
        assert_eq!(c("> - in quote"), Some(("> - ".into(), false)));
        assert_eq!(c("- "), Some(("- ".into(), true)));
        assert_eq!(c("plain text"), None);
        assert_eq!(c("-not a list"), None);
    }

    #[test]
    fn comments() {
        let l = ["  a", "", "    b"];
        let c = toggle_comment(Lang::Python, &l).unwrap();
        assert_eq!(c, vec!["  # a", "", "  #   b"]);
        let refs: Vec<&str> = c.iter().map(|s| s.as_str()).collect();
        assert_eq!(toggle_comment(Lang::Python, &refs).unwrap(), vec!["  a", "", "    b"]);
        let c = toggle_comment(Lang::Css, &["x"]).unwrap();
        assert_eq!(c, vec!["/* x */"]);
        assert_eq!(toggle_comment(Lang::Css, &["/* x */"]).unwrap(), vec!["x"]);
    }

    /// No formatter may panic on multi-byte characters, wherever they are.
    #[test]
    fn multibyte_everywhere() {
        let samples = [
            "<html>\n<!-- commento -->\n<p class=\"a\">testo</p>\n<script>\nlet s = 'x';\nif (a) { b(); }\n</script>\n<pre>\n  x\n</pre>\n</html>\n",
            "fn main() {\n    let s = \"a\"; // c\n    if x { y(); }\n}\n",
            "if [ -n \"$a\" ]; then\n  case $b in\n    x) echo 1 ;;\n  esac\nfi\n",
            "# Titolo\n\n- voce\n1. numero\n> citazione\n- [ ] task\n\n```rust\nfn a() {}\n```\n",
        ];
        for sample in samples {
            for ch in ["à", "€", "😀"] {
                for pos in (0..=sample.len()).filter(|p| sample.is_char_boundary(*p)) {
                    let text = format!("{}{ch}{}", &sample[..pos], &sample[pos..]);
                    reindent_html(&text, "  ");
                    reindent_braces(Lang::Rust, &text, "    ");
                    reindent_braces(Lang::C, &text, "    ");
                    reindent_braces(Lang::Php, &text, "    ");
                    reindent_braces(Lang::Css, &text, "    ");
                    reindent_shell(&text, "  ");
                    tidy_markdown(&text);
                    tidy(&text, Some(4));
                    let lines: Vec<&str> = text.lines().collect();
                    for lang in crate::lang::ALL {
                        toggle_comment(lang, &lines);
                    }
                    for line in &lines {
                        md_list_continuation(line);
                    }
                }
            }
        }
    }


    #[test]
    fn vue_regions() {
        let doc = "<template>\n  <p>{{ a }}</p>\n</template>\n<script setup lang=\"ts\">\nconst a = 1\n";
        assert_eq!(embedded_lang(Lang::Vue, doc), Lang::TypeScript);
        assert_eq!(embedded_lang(Lang::Vue, "<template>\n<p>"), Lang::Vue);
        assert_eq!(embedded_lang(Lang::Vue, "<script>\nx\n</script>\n<style scoped>\n"), Lang::Css);
        assert_eq!(embedded_lang(Lang::Html, "<script>\nlet x;\n"), Lang::JavaScript);
        assert_eq!(embedded_lang(Lang::Rust, "<script>"), Lang::Rust);
        let c = toggle_comment(embedded_lang(Lang::Vue, doc), &["const a = 1"]).unwrap();
        assert_eq!(c, vec!["// const a = 1"]);
        assert_eq!(toggle_comment(Lang::Vue, &["<p>x</p>"]).unwrap(), vec!["<!-- <p>x</p> -->"]);
    }

}
