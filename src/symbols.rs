//! Code symbols without a language server: definitions (functions, classes,
//! types, …) found with the lexer of each language plus a few patterns, an
//! index of the symbols of a project and helpers for identifiers.
//!
//! It knows names, not types: "find usages" of a method also finds the
//! methods with the same name of other classes.

use crate::lang::Lang;
use crate::lexer::{self, Kind};
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
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
        Lang::TypeScript | Lang::Tsx => {
            &[("function", Function), ("class", Class), ("interface", Interface), ("enum", Enum), ("type", Type), ("namespace", Module)]
        }
        Lang::Go => &[("func", Function), ("type", Type)],
        Lang::Php => &[("function", Function), ("class", Class), ("interface", Interface), ("trait", Interface), ("enum", Enum)],
        Lang::Java => &[("class", Class), ("interface", Interface), ("enum", Enum), ("record", Class)],
        Lang::CSharp => &[
            ("class", Class),
            ("struct", Struct),
            ("interface", Interface),
            ("enum", Enum),
            ("record", Class),
            ("namespace", Module),
            ("delegate", Type),
        ],
        Lang::Perl => &[("sub", Function), ("package", Module)],
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
    matches!(lang, Lang::C | Lang::Cpp | Lang::CSharp | Lang::Java | Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
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

/// `const name =` at the start of a line (module level bindings).
fn js_top_const() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:export\s+)?(?:const|let|var)\s+([\p{L}_$][\w$]*)\s*(?::[^=]+)?=").unwrap())
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
fn mask<'a>(line: &'a str, toks: &[lexer::Token]) -> Cow<'a, str> {
    if !toks.iter().any(|t| matches!(t.kind, Kind::Comment | Kind::Str)) {
        return Cow::Borrowed(line);
    }
    let mut out: Vec<u8> = line.as_bytes().to_vec();
    for t in toks.iter().filter(|t| matches!(t.kind, Kind::Comment | Kind::Str)) {
        for b in &mut out[t.start as usize..(t.end as usize).min(line.len())] {
            *b = b' ';
        }
    }
    // Spaces replace whole tokens, so the result is still valid UTF-8 except
    // when a token boundary splits a character: fall back to a lossy copy.
    Cow::Owned(String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Definitions in the `<script>` blocks of an HTML or Vue file.
fn extract_scripts(text: &str) -> Vec<Symbol> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let lower = lines[i].to_ascii_lowercase();
        let Some(open) = lower.find("<script") else {
            i += 1;
            continue;
        };
        let tag = &lower[open..lower[open..].find('>').map_or(lower.len(), |e| open + e)];
        let ts = crate::lang::script_is_ts(tag.as_bytes());
        let first = i + 1;
        let mut last = first;
        while last < lines.len() && !lines[last].to_ascii_lowercase().contains("</script") {
            last += 1;
        }
        let block = lines[first..last].join("\n");
        for mut s in extract(if ts { Lang::TypeScript } else { Lang::JavaScript }, &block) {
            s.line += first as u32;
            out.push(s);
        }
        i = last + 1;
    }
    out
}

/// `carrello-riga` → `CarrelloRiga`.
pub fn pascal_case(stem: &str) -> String {
    stem.split(['-', '_', '.'])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect()
}

/// Definitions of a file: those of `extract` and, for a Vue component,
/// the component itself (named after the file, in PascalCase).
pub fn extract_file(lang: Lang, path: &std::path::Path, text: &str) -> Vec<Symbol> {
    let mut out = extract(lang, text);
    if lang == Lang::Vue
        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
    {
        out.insert(0, Symbol { name: pascal_case(stem), kind: SymKind::Class, line: 0, col: 0 });
    }
    out
}

/// Definitions found in `text`.
pub fn extract(lang: Lang, text: &str) -> Vec<Symbol> {
    match lang {
        Lang::Vue | Lang::Html => return extract_scripts(text),
        // No definitions.
        Lang::Markdown | Lang::Css | Lang::Yaml | Lang::Plain => return Vec::new(),
        _ => {}
    }
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
        if matches!(before, lexer::State::Comment) && toks.is_empty() {
            continue;
        }
        let m = mask(line, &toks);
        let m = m.as_ref();
        let ln = n as u32;
        let mut push = |name: &str, byte: usize, kind: SymKind| {
            // `out` is sorted by line: only the symbols of this line can clash.
            if !name.is_empty() && !out.iter().rev().take_while(|s: &&Symbol| s.line == ln).any(|s| s.name == name) {
                out.push(Symbol { name: name.to_string(), kind, line: ln, col: col_of(line, byte) });
            }
        };

        if lang == Lang::Sql {
            for c in sql_def().captures_iter(m) {
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
                if let Some((start, name)) = ident_at(m, i)
                    && !is_reserved(name)
                {
                    push(name, start, kind);
                }
            }
        }

        let t = m.trim();
        // const name = (…) => / function
        if matches!(lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx) {
            for c in js_arrow().captures_iter(m) {
                let g = c.get(1).unwrap();
                push(g.as_str(), g.start(), SymKind::Function);
            }
            // Module level constants (Vue <script setup> bindings…).
            if let Some(c) = js_top_const().captures(m) {
                let g = c.get(1).unwrap();
                push(g.as_str(), g.start(), SymKind::Constant);
            }
        }
        if lang == Lang::Bash
            && let Some(c) = shell_func().captures(m)
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

/// Identifiers of `text` (for completion) starting with `prefix` (in any
/// case; `prefix` is lowercase), without duplicates.
pub fn words<'a>(text: &'a str, min_len: usize, prefix: &str) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for w in text.split(|c: char| !is_ident_char(c)) {
        if w.len() >= min_len
            && starts_with_lower(w, prefix)
            && w.chars().count() >= min_len
            && !w.starts_with(|c: char| c.is_ascii_digit())
            && seen.insert(w)
        {
            out.push(w);
        }
    }
    out
}

/// Whether `s` starts with `lower` ignoring case (`lower` is lowercase).
pub fn starts_with_lower(s: &str, lower: &str) -> bool {
    if lower.is_ascii() && s.as_bytes().get(..lower.len()).is_some_and(|b| b.is_ascii()) {
        return s.as_bytes()[..lower.len()].eq_ignore_ascii_case(lower.as_bytes());
    }
    let mut s = s.chars().flat_map(char::to_lowercase);
    lower.chars().all(|c| s.next() == Some(c))
}

// --------------------------------------------------------------- project

/// Borrowed view of a symbol stored in [`ProjectSymbols`].
#[derive(Clone, Copy, Debug)]
pub struct EntryRef<'a> {
    pub name: &'a str,
    pub kind: SymKind,
    pub line: u32,
    pub col: u32,
    /// Index of the file in `ProjectSymbols::files`.
    pub file: u32,
}

impl EntryRef<'_> {
    pub fn to_symbol(self) -> Symbol {
        Symbol { name: self.name.to_string(), kind: self.kind, line: self.line, col: self.col }
    }
}

/// Compact index entry pointing into a shared UTF-8 string arena (`blob`):
/// `blob[off .. off + key_len]` is the lowercase key; when `name_len == 0`
/// the symbol name equals its key, otherwise `blob[off + key_len .. off +
/// key_len + name_len]` holds the original-case name.
#[derive(Clone, Copy, Debug)]
struct StoredEntry {
    off: u32,
    key_len: u32,
    name_len: u32,
    line: u32,
    col: u32,
    file: u32,
    kind: SymKind,
}

impl StoredEntry {
    fn key<'a>(&self, blob: &'a str) -> &'a str {
        let s = self.off as usize;
        &blob[s..s + self.key_len as usize]
    }

    fn name<'a>(&self, blob: &'a str) -> &'a str {
        if self.name_len == 0 {
            self.key(blob)
        } else {
            let s = (self.off + self.key_len) as usize;
            &blob[s..s + self.name_len as usize]
        }
    }

    fn raw_slice<'a>(&self, blob: &'a str) -> &'a str {
        let s = self.off as usize;
        let len = (self.key_len + self.name_len) as usize;
        &blob[s..s + len]
    }

    fn as_ref<'a>(&self, blob: &'a str) -> EntryRef<'a> {
        EntryRef { name: self.name(blob), kind: self.kind, line: self.line, col: self.col, file: self.file }
    }
}

fn push_symbol(blob: &mut String, sym: Symbol, file: u32) -> StoredEntry {
    let off = blob.len() as u32;
    let is_lower = if sym.name.is_ascii() {
        !sym.name.bytes().any(|b| b.is_ascii_uppercase())
    } else {
        !sym.name.chars().any(char::is_uppercase)
    };
    let (key_len, name_len) = if is_lower {
        blob.push_str(&sym.name);
        (sym.name.len() as u32, 0)
    } else {
        if sym.name.is_ascii() {
            for b in sym.name.bytes() {
                blob.push(b.to_ascii_lowercase() as char);
            }
        } else {
            blob.extend(sym.name.chars().flat_map(char::to_lowercase));
        }
        let key_len = (blob.len() as u32) - off;
        if blob[off as usize..] == sym.name {
            (key_len, 0)
        } else {
            blob.push_str(&sym.name);
            (key_len, sym.name.len() as u32)
        }
    };
    StoredEntry { off, key_len, name_len, line: sym.line, col: sym.col, file, kind: sym.kind }
}

/// Modification time (ns) and size of a file: its symbols are read again
/// only when this changes.
pub type Stamp = (u128, u64);

pub fn stamp_of(m: &std::fs::Metadata) -> Stamp {
    let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    (t, m.len())
}

/// Symbols of a file after a new scan of the folder.
pub enum FileSymbols {
    /// Unchanged since the last scan: the file had this index.
    Same(u32),
    New(Vec<Symbol>),
}

/// Compact arena of newly scanned symbols sorted by lowercase name.
#[derive(Default)]
pub struct Sorted {
    blob: String,
    entries: Vec<StoredEntry>,
}

/// Sorts the new symbols (done in the background before `apply_scan`).
pub fn sort_new(per_file: &mut [FileSymbols]) -> Sorted {
    let (mut count, mut bytes) = (0usize, 0usize);
    for fs in per_file.iter() {
        if let FileSymbols::New(syms) = fs {
            count += syms.len();
            bytes += syms.iter().map(|s| s.name.len() * 2).sum::<usize>();
        }
    }
    let mut blob = String::with_capacity(bytes);
    let mut entries = Vec::with_capacity(count);
    for (f, fs) in per_file.iter_mut().enumerate() {
        if let FileSymbols::New(syms) = fs {
            for sym in std::mem::take(syms) {
                entries.push(push_symbol(&mut blob, sym, f as u32));
            }
        }
    }
    entries.sort_unstable_by(|a, b| a.key(&blob).cmp(b.key(&blob)).then(a.file.cmp(&b.file)).then(a.line.cmp(&b.line)));
    blob.shrink_to_fit();
    Sorted { blob, entries }
}

/// Definitions of the files of a folder, stored in two contiguous arenas
/// (`files` and `blob` + `entries`) sorted by lower case name.
#[derive(Default)]
pub struct ProjectSymbols {
    pub files: crate::finder::Paths,
    stamps: Vec<Stamp>,
    blob: String,
    entries: Vec<StoredEntry>,
}

impl ProjectSymbols {
    #[cfg(test)]
    pub fn build(files: Vec<String>, per_file: Vec<Vec<Symbol>>) -> ProjectSymbols {
        let stamps = vec![(0, 0); files.len()];
        let files = crate::finder::Paths::from_vec(&files);
        let mut per_file: Vec<FileSymbols> = per_file.into_iter().map(FileSymbols::New).collect();
        let sorted = sort_new(&mut per_file);
        let mut p = ProjectSymbols::default();
        p.apply_scan(files, stamps, &per_file, sorted);
        p
    }

    /// Files and stamps of the last scan: file → (stamp, index).
    pub fn stamps(&self) -> HashMap<String, (Stamp, u32)> {
        let mut out = HashMap::with_capacity(self.files.len());
        for (i, &s) in self.stamps.iter().enumerate() {
            out.insert(self.files.get(i).to_string(), (s, i as u32));
        }
        out
    }

    /// Whether a scan found the same files, all unchanged.
    pub fn unchanged(&self, files: &crate::finder::Paths, per_file: &[FileSymbols]) -> bool {
        files.len() == self.files.len() && per_file.iter().enumerate().all(|(i, f)| matches!(f, FileSymbols::Same(o) if *o as usize == i))
    }

    /// Takes the result of a new scan: symbols of unchanged files are kept,
    /// `sorted` (from `sort_new`) replaces the others.
    pub fn apply_scan(&mut self, files: crate::finder::Paths, stamps: Vec<Stamp>, per_file: &[FileSymbols], sorted: Sorted) {
        let mut remap = vec![None; self.files.len()];
        let mut any_kept = false;
        for (new, f) in per_file.iter().enumerate() {
            if let FileSymbols::Same(old) = f {
                remap[*old as usize] = Some(new as u32);
                any_kept = true;
            }
        }
        if !any_kept || self.entries.is_empty() {
            self.blob = sorted.blob;
            self.entries = sorted.entries;
        } else {
            self.merge(|f| remap.get(f as usize).copied().flatten(), sorted);
        }
        self.files = files;
        self.stamps = stamps;
    }

    /// Keeps the entries whose file `keep` maps to a (new) index, and adds
    /// `new`, both sorted: a linear merge into a fresh compact arena.
    fn merge(&mut self, keep: impl Fn(u32) -> Option<u32>, new: Sorted) {
        let old_blob = std::mem::take(&mut self.blob);
        let old_entries = std::mem::take(&mut self.entries);
        let mut merged_blob = String::with_capacity(old_blob.len() + new.blob.len());
        let mut merged_entries = Vec::with_capacity(old_entries.len() + new.entries.len());

        let mut old = old_entries
            .into_iter()
            .filter_map(|mut e| {
                e.file = keep(e.file)?;
                Some(e)
            })
            .peekable();
        let mut added = new.entries.into_iter().peekable();
        loop {
            let take_old = match (old.peek(), added.peek()) {
                (Some(a), Some(b)) => a.key(&old_blob) <= b.key(&new.blob),
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            let (mut e, src_blob) = if take_old {
                (old.next().expect("peeked"), old_blob.as_str())
            } else {
                (added.next().expect("peeked"), new.blob.as_str())
            };
            let raw = e.raw_slice(src_blob);
            e.off = merged_blob.len() as u32;
            merged_blob.push_str(raw);
            merged_entries.push(e);
        }
        if merged_blob.capacity() > merged_blob.len() + (64 << 10) {
            merged_blob.shrink_to_fit();
        }
        self.blob = merged_blob;
        self.entries = merged_entries;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Definitions named exactly `name`.
    pub fn find<'a>(&'a self, name: &str) -> Vec<EntryRef<'a>> {
        let key: Cow<'_, str> = if name.is_ascii() {
            if name.bytes().any(|b| b.is_ascii_uppercase()) {
                Cow::Owned(name.to_ascii_lowercase())
            } else {
                Cow::Borrowed(name)
            }
        } else {
            Cow::Owned(name.to_lowercase())
        };
        let start = self.entries.partition_point(|e| e.key(&self.blob) < key.as_ref());
        self.entries[start..]
            .iter()
            .take_while(|e| e.key(&self.blob) == key.as_ref())
            .filter(|e| e.name(&self.blob) == name)
            .map(|e| e.as_ref(&self.blob))
            .collect()
    }

    /// Symbols whose name starts with `prefix` (ignoring case).
    pub fn with_prefix<'a>(&'a self, prefix: &str, limit: usize) -> Vec<EntryRef<'a>> {
        let key: Cow<'_, str> = if prefix.is_ascii() {
            if prefix.bytes().any(|b| b.is_ascii_uppercase()) {
                Cow::Owned(prefix.to_ascii_lowercase())
            } else {
                Cow::Borrowed(prefix)
            }
        } else {
            Cow::Owned(prefix.to_lowercase())
        };
        let start = self.entries.partition_point(|e| e.key(&self.blob) < key.as_ref());
        self.entries[start..]
            .iter()
            .take_while(|e| e.key(&self.blob).starts_with(key.as_ref()))
            .map(|e| e.as_ref(&self.blob))
            .take(limit)
            .collect()
    }

    /// Replaces the symbols of `file` (after it was saved).
    pub fn update_file(&mut self, rel: &str, stamp: Stamp, syms: Vec<Symbol>) {
        let idx = match self.files.position(rel) {
            Some(i) => {
                self.stamps[i] = stamp;
                i as u32
            }
            None => {
                let i = self.files.push(rel);
                self.stamps.push(stamp);
                i as u32
            }
        };
        let bytes: usize = syms.iter().map(|s| s.name.len() * 2).sum();
        let mut blob = String::with_capacity(bytes);
        let mut entries: Vec<StoredEntry> = syms.into_iter().map(|sym| push_symbol(&mut blob, sym, idx)).collect();
        entries.sort_unstable_by(|a, b| a.key(&blob).cmp(b.key(&blob)).then(a.line.cmp(&b.line)));
        self.merge(|f| (f != idx).then_some(f), Sorted { blob, entries });
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
    fn vue_component() {
        let src = "<template>\n  <p @click=\"aggiungi\">{{ conteggio }}</p>\n</template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nconst conteggio = ref(0)\nfunction aggiungi() {\n  conteggio.value++\n}\n</script>\n<style scoped>\np { color: red }\n</style>\n";
        let n: Vec<(String, SymKind, u32)> =
            extract_file(Lang::Vue, std::path::Path::new("src/carrello-riga.vue"), src).into_iter().map(|s| (s.name, s.kind, s.line)).collect();
        assert_eq!(
            n,
            vec![
                ("CarrelloRiga".into(), SymKind::Class, 0),
                ("conteggio".into(), SymKind::Constant, 5),
                ("aggiungi".into(), SymKind::Function, 6)
            ]
        );
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
        p.update_file("b.rs", (0, 0), vec![]);
        assert_eq!(p.find("apri").len(), 1);
        // A new scan: a.rs unchanged (moved to index 1), c.rs new, b.rs gone.
        let mut per_file = vec![FileSymbols::New(extract(Lang::Rust, "fn apri() {}\n")), FileSymbols::Same(0)];
        let sorted = sort_new(&mut per_file);
        p.apply_scan(crate::finder::Paths::from_vec(&["c.rs".into(), "a.rs".into()]), vec![(1, 1), (0, 0)], &per_file, sorted);
        let found = p.find("apri");
        let files: Vec<&str> = found.iter().map(|e| p.files.get(e.file as usize)).collect();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"a.rs") && files.contains(&"c.rs"));
        assert_eq!(p.with_prefix("a", 10).len(), 3);
        assert!(p.entries.windows(2).all(|w| w[0].key(&p.blob) <= w[1].key(&p.blob)));
        assert!(p.files.get(0) == "c.rs");
        // Adding a brand-new file with mixed-case and Unicode symbols via update_file.
        p.update_file("nuovo.py", (2, 2), extract(Lang::Python, "class CittàNuova:\n    def saluta():\n        pass\n"));
        let cn = p.find("CittàNuova");
        assert_eq!(cn.len(), 1);
        assert_eq!(p.files.get(cn[0].file as usize), "nuovo.py");
        assert_eq!(p.with_prefix("CITTÀ", 10).len(), 1);
        assert_eq!(words("a bb cc_d bb 9x é_é", 2, ""), vec!["bb", "cc_d", "é_é"]);
        assert_eq!(words("Foo fob bar FOOD", 2, "fo"), vec!["Foo", "fob", "FOOD"]);
        assert!(starts_with_lower("CittàVecchia", "città"));
        assert!(starts_with_lower("İstanbul", "i"));
        assert!(!starts_with_lower("f", "fo"));
    }
}

