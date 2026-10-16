//! Code symbols without a language server: definitions (functions, classes,
//! types, …) found with the lexer of each language plus a few patterns, an
//! index of the symbols of a project and helpers for identifiers.
//!
//! It knows names, not types: "find usages" of a method also finds the
//! methods with the same name of other classes.

use crate::lang::Lang;
use crate::lexer::{self, Kind};
use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymKind {
    Function,
    Class,
    Struct,
    Enum,
    Interface,
    Type,
    Module,
    Constant,
    Macro,
    Table,
}

impl SymKind {
    pub fn label(self) -> &'static str {
        match self {
            SymKind::Function => "funzione",
            SymKind::Class => "classe",
            SymKind::Struct => "struct",
            SymKind::Enum => "enum",
            SymKind::Interface => "interfaccia",
            SymKind::Type => "tipo",
            SymKind::Module => "modulo",
            SymKind::Constant => "costante",
            SymKind::Macro => "macro",
            SymKind::Table => "tabella",
        }
    }
}

/// A definition in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymKind,
    /// Line (from 0) and column (in characters).
    pub line: u32,
    pub col: u32,
}

pub fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Keyword introducing a definition and the kind of what it defines.
fn def_keywords(lang: Lang) -> &'static [(&'static str, SymKind)] {
    use SymKind::*;
    match lang {
        Lang::Rust => &[
            ("fn", Function),
            ("struct", Struct),
            ("enum", Enum),
            ("union", Struct),
            ("trait", Interface),
            ("type", Type),
            ("mod", Module),
            ("const", Constant),
            ("static", Constant),
            ("macro_rules!", Macro),
        ],
        Lang::Python => &[("def", Function), ("class", Class)],
        Lang::JavaScript => &[("function", Function), ("class", Class)],
        Lang::TypeScript => {
            &[("function", Function), ("class", Class), ("interface", Interface), ("enum", Enum), ("type", Type), ("namespace", Module)]
        }
        Lang::Go => &[("func", Function), ("type", Type)],
        Lang::Php => &[("function", Function), ("class", Class), ("interface", Interface), ("trait", Interface), ("enum", Enum)],
        Lang::Java => &[("class", Class), ("interface", Interface), ("enum", Enum), ("record", Class)],
        Lang::C | Lang::Cpp => &[
            ("struct", Struct),
            ("union", Struct),
            ("enum", Enum),
            ("class", Class),
            ("namespace", Module),
            ("#define", Macro),
        ],
        Lang::Bash => &[("function", Function)],
        _ => &[],
    }
}

/// Languages where `name(…) {` at the start of a line defines a function
/// or method (C, C++, Java, JavaScript class methods…).
fn c_like_functions(lang: Lang) -> bool {
    matches!(lang, Lang::C | Lang::Cpp | Lang::Java | Lang::JavaScript | Lang::TypeScript)
}

fn control_words() -> &'static [&'static str] {
    &[
        "if", "for", "while", "switch", "catch", "return", "else", "do", "sizeof", "new", "delete", "throw", "case",
        "function", "typeof", "await", "yield", "elif", "when", "synchronized",
    ]
}

fn js_arrow() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[\s;])(?:export\s+)?(?:const|let|var)\s+([\p{L}_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[\p{L}_$][\w$]*\s*=>)")
            .unwrap()
    })
}

fn sql_def() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)\bcreate\s+(?:or\s+replace\s+)?(?:temp(?:orary)?\s+|unique\s+|materialized\s+)?(table|view|function|procedure|trigger|index|type|sequence|schema)\s+(?:if\s+not\s+exists\s+)?([\w."`]+)"#)
            .unwrap()
    })
}

fn shell_func() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*([A-Za-z_][\w:.-]*)\s*\(\s*\)").unwrap())
}

/// The identifier starting at byte `i` of `s` (after spaces).
fn ident_at(s: &str, i: usize) -> Option<(usize, &str)> {
    let rest = &s[i..];
    let skip = rest.len() - rest.trim_start().len();
    let start = i + skip;
    let end = s[start..].char_indices().find(|(_, c)| !is_ident_char(*c)).map_or(s.len(), |(j, _)| start + j);
    let id = &s[start..end];
    (!id.is_empty() && !id.starts_with(|c: char| c.is_ascii_digit())).then_some((start, id))
}

fn col_of(line: &str, byte: usize) -> u32 {
    line[..byte].chars().count() as u32
}

/// `line` with comments and strings replaced by spaces.
fn mask(line: &str, toks: &[lexer::Token]) -> String {
    let mut out: Vec<u8> = line.as_bytes().to_vec();
    for t in toks.iter().filter(|t| matches!(t.kind, Kind::Comment | Kind::Str)) {
        for b in &mut out[t.start as usize..(t.end as usize).min(line.len())] {
            *b = b' ';
        }
    }
    // Spaces replace whole tokens, so the result is still valid UTF-8 except
    // when a token boundary splits a character: fall back to a lossy copy.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Definitions found in `text`.
pub fn extract(lang: Lang, text: &str) -> Vec<Symbol> {
    let keywords = def_keywords(lang);
    let reserved = lang.spec();
    let is_reserved = |w: &str| reserved.keywords.contains(&w) || reserved.types.contains(&w);
    let is_control = |w: &str| control_words().contains(&w);
    let mut out = Vec::new();
    let mut state = lexer::initial_state(lang);
    let mut toks = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (n, line) in lines.iter().enumerate() {
        let before = state;
        state = lexer::lex_line(lang, state, line, &mut toks);
        if matches!(lang, Lang::Html | Lang::Markdown | Lang::Css | Lang::Yaml | Lang::Plain)
            || (matches!(before, lexer::State::Comment) && toks.is_empty())
        {
            continue;
        }
        let m = mask(line, &toks);
        let ln = n as u32;
        let mut push = |name: &str, byte: usize, kind: SymKind| {
            if !name.is_empty() && !out.iter().any(|s: &Symbol| s.line == ln && s.name == name) {
                out.push(Symbol { name: name.to_string(), kind, line: ln, col: col_of(line, byte) });
            }
        };

        if lang == Lang::Sql {
            for c in sql_def().captures_iter(&m) {
                let g = c.get(2).unwrap();
                let name = g.as_str().rsplit('.').next().unwrap_or("").trim_matches(|c| c == '"' || c == '`');
                let kind = match c[1].to_ascii_lowercase().as_str() {
                    "table" | "view" => SymKind::Table,
                    "function" | "procedure" | "trigger" => SymKind::Function,
                    "type" => SymKind::Type,
                    _ => SymKind::Constant,
                };
                let offset = g.as_str().len() - name.len() - (g.as_str().ends_with(['"', '`']) as usize);
                push(name, g.start() + offset.min(g.as_str().len()), kind);
            }
            continue;
        }

        // keyword name
        for &(kw, kind) in keywords {
            let mut from = 0;
            while let Some(p) = m[from..].find(kw).map(|p| p + from) {
                from = p + kw.len();
                let ok_before = p == 0 || !m[..p].ends_with(is_ident_char);
                let ok_after = kw.ends_with('!') || m[from..].starts_with(char::is_whitespace);
                if !ok_before || !ok_after {
                    continue;
                }
                let mut i = from;
                // Go methods: func (r *T) Name(
                if kw == "func" && m[i..].trim_start().starts_with('(') {
                    let open = i + m[i..].find('(').unwrap();
                    match m[open..].find(')') {
                        Some(c) => i = open + c + 1,
                        None => continue,
                    }
                }
                if let Some((start, name)) = ident_at(&m, i)
                    && !is_reserved(name)
                {
                    push(name, start, kind);
                }
            }
        }

        let t = m.trim();
        // const name = (…) => / function
        if matches!(lang, Lang::JavaScript | Lang::TypeScript) {
            for c in js_arrow().captures_iter(&m) {
                let g = c.get(1).unwrap();
                push(g.as_str(), g.start(), SymKind::Function);
            }
        }
        if lang == Lang::Bash
            && let Some(c) = shell_func().captures(&m)
        {
            let g = c.get(1).unwrap();
            push(g.as_str(), g.start(), SymKind::Function);
        }
        // name(…) { at the start of a line
        if c_like_functions(lang) && !t.ends_with(';') && !t.starts_with('}') && !t.starts_with('#') {
            let Some(open) = m.find('(') else { continue };
            let head = &m[..open];
            let name_start = head.trim_end().rfind(|c: char| !is_ident_char(c) && c != ':' && c != '~').map_or(0, |p| p + 1);
            let qualified = head[name_start..].trim();
            let name = qualified.rsplit("::").next().unwrap_or("").trim_start_matches('~');
            let prefix = head[..name_start].trim();
            let first = prefix.split_whitespace().next().unwrap_or(name);
            let prefix_ok = prefix.chars().all(|c| is_ident_char(c) || " \t*&<>,[]:~".contains(c));
            let next_line = lines.get(n + 1).map_or("", |l| l.trim());
            let body = m[open..].contains('{') || next_line.starts_with('{') || (m.trim_end().ends_with(')') && next_line.starts_with(':'));
            let needs_type = lang == Lang::C;
            if !name.is_empty()
                && prefix_ok
                && body
                && (!needs_type || !prefix.is_empty())
                && !is_reserved(name)
                && !is_control(name)
                && !is_control(first)
                && !name.starts_with(|c: char| c.is_ascii_digit())
            {
                let byte = name_start + head[name_start..].len() - head[name_start..].trim_start().len() + qualified.len() - name.len();
                push(name, byte.min(line.len()), SymKind::Function);
            }
        }
    }
    out
}

/// Identifiers of `text` (for completion), without duplicates.
pub fn words(text: &str, min_len: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for w in text.split(|c: char| !is_ident_char(c)) {
        if w.chars().count() >= min_len && !w.starts_with(|c: char| c.is_ascii_digit()) && seen.insert(w) {
            out.push(w.to_string());
        }
    }
    out
}

// --------------------------------------------------------------- project

/// A symbol of the project.
#[derive(Clone, Debug)]
pub struct Entry {
    pub sym: Symbol,
    /// Path relative to the folder.
    pub file: u32,
}

/// Definitions of the files of a folder, sorted by lower case name.
#[derive(Default)]
pub struct ProjectSymbols {
    pub files: Vec<String>,
    entries: Vec<Entry>,
    keys: Vec<String>,
}

impl ProjectSymbols {
    pub fn build(files: Vec<String>, per_file: Vec<Vec<Symbol>>) -> ProjectSymbols {
        let mut entries: Vec<Entry> = per_file
            .into_iter()
            .enumerate()
            .flat_map(|(f, syms)| syms.into_iter().map(move |sym| Entry { sym, file: f as u32 }))
            .collect();
        entries.sort_by_cached_key(|e| e.sym.name.to_lowercase());
        let keys = entries.iter().map(|e| e.sym.name.to_lowercase()).collect();
        ProjectSymbols { files, entries, keys }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Definitions named exactly `name`.
    pub fn find(&self, name: &str) -> Vec<&Entry> {
        let key = name.to_lowercase();
        let start = self.keys.partition_point(|k| k.as_str() < key.as_str());
        self.entries[start..]
            .iter()
            .zip(&self.keys[start..])
            .take_while(|(_, k)| **k == key)
            .filter(|(e, _)| e.sym.name == name)
            .map(|(e, _)| e)
            .collect()
    }

    /// Symbols whose name starts with `prefix` (ignoring case).
    pub fn with_prefix(&self, prefix: &str, limit: usize) -> Vec<&Entry> {
        let key = prefix.to_lowercase();
        let start = self.keys.partition_point(|k| k.as_str() < key.as_str());
        self.entries[start..]
            .iter()
            .zip(&self.keys[start..])
            .take_while(|(_, k)| k.starts_with(key.as_str()))
            .map(|(e, _)| e)
            .take(limit)
            .collect()
    }

    /// Replaces the symbols of `file` (after it was saved).
    pub fn update_file(&mut self, rel: &str, syms: Vec<Symbol>) {
        let idx = match self.files.iter().position(|f| f == rel) {
            Some(i) => i as u32,
            None => {
                self.files.push(rel.to_string());
                (self.files.len() - 1) as u32
            }
        };
        let mut all: Vec<Entry> = std::mem::take(&mut self.entries).into_iter().filter(|e| e.file != idx).collect();
        all.extend(syms.into_iter().map(|sym| Entry { sym, file: idx }));
        all.sort_by_cached_key(|e| e.sym.name.to_lowercase());
        self.keys = all.iter().map(|e| e.sym.name.to_lowercase()).collect();
        self.entries = all;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(lang: Lang, text: &str) -> Vec<(String, SymKind, u32, u32)> {
        extract(lang, text).into_iter().map(|s| (s.name, s.kind, s.line, s.col)).collect()
    }

    #[test]
    fn rust() {
        let src = "pub struct Doc {\n    x: u32,\n}\nimpl Doc {\n    pub fn new() -> Doc { Doc { x: 0 } }\n    // fn commented()\n    const fn limit() {}\n}\nmacro_rules! sym { () => {} }\nlet s = \"fn nope()\";\n";
        let n = names(Lang::Rust, src);
        assert!(n.contains(&("Doc".into(), SymKind::Struct, 0, 11)));
        assert!(n.contains(&("new".into(), SymKind::Function, 4, 11)));
        assert!(n.contains(&("limit".into(), SymKind::Function, 6, 13)));
        assert!(n.contains(&("sym".into(), SymKind::Macro, 8, 13)));
        assert!(!n.iter().any(|x| x.0 == "commented" || x.0 == "nope" || x.0 == "fn"));
    }

    #[test]
    fn others() {
        let py = names(Lang::Python, "class Città:\n    def saluta(self):\n        pass\n");
        assert_eq!(py, vec![("Città".into(), SymKind::Class, 0, 6), ("saluta".into(), SymKind::Function, 1, 8)]);
        let go = names(Lang::Go, "func (r *T) Name() error {\n}\ntype T struct{}\nfunc main() {}\n");
        assert_eq!(go.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["Name", "T", "main"]);
        let c = names(Lang::C, "static int conta(const char *s)\n{\n    if (x) {\n    foo(1);\n}\nstruct punto { int x; };\n#define MAX 10\n");
        assert_eq!(c.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["conta", "punto", "MAX"]);
        let cpp = names(Lang::Cpp, "void Editor::open(const std::string& p) {\n}\nEditor::~Editor() {\n}\n");
        assert_eq!(cpp.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["open", "Editor"]);
        let java = names(Lang::Java, "public class App {\n  public static void main(String[] args) {\n    if (a) {\n    }\n  }\n}\n");
        assert_eq!(java.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["App", "main"]);
        let js = names(Lang::JavaScript, "export const somma = (a, b) => a + b;\nclass Carrello {\n  aggiungi(x) {\n  }\n}\nfunction totale() {}\n");
        assert_eq!(js.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["somma", "Carrello", "aggiungi", "totale"]);
        let sql = names(Lang::Sql, "CREATE TABLE IF NOT EXISTS public.utenti (\n);\ncreate or replace function conta_ordini() returns int\n");
        assert_eq!(sql.iter().map(|x| (x.0.as_str(), x.1)).collect::<Vec<_>>(), vec![("utenti", SymKind::Table), ("conta_ordini", SymKind::Function)]);
        let sh = names(Lang::Bash, "build() {\n  echo\n}\nfunction pulisci {\n}\n");
        assert_eq!(sh.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["build", "pulisci"]);
    }

    #[test]
    fn project_index() {
        let p = ProjectSymbols::build(
            vec!["a.rs".into(), "b.rs".into()],
            vec![extract(Lang::Rust, "fn apri() {}\nfn Apri_tutto() {}\n"), extract(Lang::Rust, "fn apri() {}\nstruct Altro;\n")],
        );
        assert_eq!(p.find("apri").len(), 2);
        assert_eq!(p.with_prefix("ap", 10).len(), 3);
        assert!(p.find("Apri").is_empty());
        let mut p = p;
        p.update_file("b.rs", vec![]);
        assert_eq!(p.find("apri").len(), 1);
        assert_eq!(words("a bb cc_d bb 9x é_é", 2), vec!["bb", "cc_d", "é_é"]);
    }
}

