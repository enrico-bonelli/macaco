//! Quick file search in the working folder: list of the files of a folder
//! (from git or a walk of the folder), fuzzy matching and the on-disk cache
//! of the indexed folders.
//!
//! Files inside hidden folders (`.git`, `.cache`, …) are never listed; in a
//! git repository the files ignored by git are left out too.

use crate::config;
use crate::encoding;
use crate::search::Query;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Do not list more files than this.
const MAX_FILES: usize = 1_000_000;

/// Whether the relative path `rel` lies inside a hidden folder.
fn in_hidden_dir(rel: &str) -> bool {
    rel.split('/').rev().skip(1).any(|c| c.starts_with('.'))
}

/// List of relative paths stored in one block of text (`a\0b\0…`): half
/// the memory of a `Vec<String>`, and the same format as the cache file.
#[derive(Default)]
pub struct Paths {
    data: String,
    /// Start and end of each path in `data`.
    spans: Vec<(u32, u32)>,
}

impl Paths {
    fn from_blob(data: String) -> Paths {
        let mut spans = Vec::new();
        let mut at = 0;
        for (i, b) in data.bytes().chain(std::iter::once(0)).enumerate() {
            if b == 0 {
                if i > at {
                    spans.push((at as u32, i as u32));
                }
                at = i + 1;
            }
        }
        Paths { data, spans }
    }

    fn from_vec(v: &[String]) -> Paths {
        let mut data = String::with_capacity(v.iter().map(|p| p.len() + 1).sum());
        for p in v {
            data.push_str(p);
            data.push('\0');
        }
        Paths::from_blob(data)
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn get(&self, i: usize) -> &str {
        let (start, end) = self.spans[i];
        &self.data[start as usize..end as usize]
    }
}

/// Files of `root` as paths relative to it, sorted. Slow on big folders:
/// to be run in a thread.
pub fn scan(root: &Path) -> Paths {
    let mut v = git_files(root).unwrap_or_else(|| walk(root));
    v.retain(|p| !in_hidden_dir(p));
    v.sort_unstable();
    v.dedup();
    Paths::from_vec(&v)
}

/// Tracked and untracked (not ignored) files, if `root` is in a repository.
fn git_files(root: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .take(MAX_FILES)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// Walks `root` skipping hidden folders; links to folders are not followed.
/// Folders that are git repositories are listed by git, so that the files
/// they ignore (`target/`, `node_modules/`…) are left out.
fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        if !rel.as_os_str().is_empty()
            && dir.join(".git").exists()
            && let Some(files) = git_files(&dir)
        {
            let prefix = rel.to_string_lossy();
            out.extend(files.into_iter().map(|f| format!("{prefix}/{f}")));
            if out.len() >= MAX_FILES {
                out.truncate(MAX_FILES);
                return out;
            }
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name();
            let r = rel.join(&name);
            if ft.is_dir() {
                if !name.to_string_lossy().starts_with('.') {
                    stack.push(r);
                }
            } else if ft.is_file() || (ft.is_symlink() && fs::metadata(root.join(&r)).is_ok_and(|m| m.is_file())) {
                out.push(r.to_string_lossy().into_owned());
                if out.len() >= MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------- matching

/// A file matching the query: index in the list, score and positions (in
/// characters) of the matched characters.
pub struct Hit {
    pub index: usize,
    pub positions: Vec<usize>,
}

/// The query prepared for matching: lower case, without spaces.
pub fn normalize(query: &str) -> String {
    query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

fn is_separator(c: u8) -> bool {
    matches!(c, b'/' | b'_' | b'-' | b'.' | b' ')
}

/// Greedy case-insensitive match of `q` in `s` from byte `from`, without
/// allocating: the byte positions go to `out`.
fn greedy(q: &[u8], s: &[u8], from: usize, out: &mut Vec<usize>) -> bool {
    out.clear();
    let mut i = from;
    for &c in q {
        while i < s.len() && s[i].to_ascii_lowercase() != c {
            i += 1;
        }
        if i == s.len() {
            return false;
        }
        out.push(i);
        i += 1;
    }
    true
}

/// Score of `s` for the normalized query `q`; the byte positions of the
/// matched characters are left in `pos`.
fn score_bytes(q: &[u8], s: &[u8], pos: &mut Vec<usize>) -> Option<i32> {
    // Most paths fail here, at the cost of one pass over their bytes.
    if !greedy(q, s, 0, pos) {
        return None;
    }
    let name_start = s.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
    // Prefer a match inside the file name.
    if pos[0] < name_start {
        let mut in_name = Vec::with_capacity(q.len());
        if greedy(q, s, name_start, &mut in_name) {
            *pos = in_name;
        }
    }
    let mut score = 0;
    let mut prev = usize::MAX;
    let mut run = 0;
    for &p in pos.iter() {
        score += 1;
        if p >= name_start {
            score += 2;
        }
        if prev != usize::MAX && p == prev + 1 {
            run += 1;
            score += 4 * run;
        } else {
            run = 0;
        }
        if p == 0 || is_separator(s[p - 1]) || (s[p].is_ascii_uppercase() && s[p - 1].is_ascii_lowercase()) {
            score += 6;
        }
        prev = p;
    }
    let name = &s[name_start..];
    if name.len() >= q.len() && name[..q.len()].eq_ignore_ascii_case(q) {
        score += 20;
        let stem = name.iter().position(|&c| c == b'.').map_or(name.len(), |i| i.max(1));
        if stem == q.len() || name.len() == q.len() {
            score += 30;
        }
    }
    Some(score - (s.len() as i32) / 10)
}

/// Score of `rel` for `query` and the positions (in characters) of the
/// matched characters; None if it does not match.
#[cfg(test)]
fn score(query: &str, rel: &str) -> Option<(i32, Vec<usize>)> {
    let q = normalize(query);
    let mut pos = Vec::new();
    let s = score_bytes(q.as_bytes(), rel.as_bytes(), &mut pos)?;
    Some((s, char_positions(rel, &pos)))
}

/// Byte positions to character positions.
fn char_positions(s: &str, bytes: &[usize]) -> Vec<usize> {
    if s.is_ascii() {
        return bytes.to_vec();
    }
    bytes.iter().map(|&b| s[..b].chars().count()).collect()
}

/// Result of a search: the best matches and every matching index (to
/// narrow the next search when the query grows).
pub struct Found {
    pub hits: Vec<Hit>,
    pub all: Vec<u32>,
}

/// The best `limit` matches of `query`. With `within`, only those indices
/// are considered (the matches of a shorter query).
pub fn search(paths: &Paths, query: &str, limit: usize, within: Option<&[u32]>) -> Found {
    let q = normalize(query);
    if q.is_empty() {
        let hits = (0..paths.len().min(limit)).map(|index| Hit { index, positions: Vec::new() }).collect();
        return Found { hits, all: Vec::new() };
    }
    let q = q.as_bytes();
    let mut pos = Vec::with_capacity(q.len());
    let mut scored: Vec<(i32, u32)> = Vec::new();
    let mut check = |i: u32| {
        if let Some(s) = score_bytes(q, paths.get(i as usize).as_bytes(), &mut pos) {
            scored.push((s, i));
        }
    };
    match within {
        Some(idx) => idx.iter().for_each(|&i| check(i)),
        None => (0..paths.len() as u32).for_each(&mut check),
    }
    let all: Vec<u32> = scored.iter().map(|&(_, i)| i).collect();
    let order = |a: &(i32, u32), b: &(i32, u32)| {
        b.0.cmp(&a.0).then_with(|| paths.get(a.1 as usize).len().cmp(&paths.get(b.1 as usize).len()))
    };
    if scored.len() > limit {
        scored.select_nth_unstable_by(limit, order);
        scored.truncate(limit);
    }
    scored.sort_unstable_by(order);
    let hits = scored
        .into_iter()
        .map(|(_, i)| {
            let rel = paths.get(i as usize);
            let mut bytes = Vec::new();
            score_bytes(q, rel.as_bytes(), &mut bytes);
            Hit { index: i as usize, positions: char_positions(rel, &bytes) }
        })
        .collect();
    Found { hits, all }
}

// ---------------------------------------------------------- text search

/// Files bigger than this are not searched.
const MAX_SEARCH_FILE: u64 = 8 << 20;
/// Stop after this many matching lines.
pub const MAX_TEXT_MATCHES: usize = 10_000;
/// Characters of a matching line shown before and after the match.
const SNIPPET_BEFORE: usize = 60;
const SNIPPET_AFTER: usize = 120;

/// A line containing the text searched.
pub struct TextMatch {
    /// Index in `TextResults::files`.
    pub file: u32,
    /// Line (from 0) and column (in characters) of the first match.
    pub line: u32,
    pub col: u32,
    /// Part of the line to show and the match inside it (in characters).
    pub snippet: String,
    pub hl: (u32, u32),
}

#[derive(Default)]
pub struct TextResults {
    /// Files with matches, relative to the folder.
    pub files: Vec<String>,
    pub matches: Vec<TextMatch>,
    /// Files searched.
    pub searched: usize,
    /// More matches than `MAX_TEXT_MATCHES`.
    pub truncated: bool,
    pub cancelled: bool,
}

/// Progress of a search, shared with the thread running it.
#[derive(Default)]
pub struct Progress {
    pub cancel: AtomicBool,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
}

fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8000)];
    let utf16 = head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]);
    !utf16 && head.contains(&0)
}

/// Text of a file as the editor shows it (encoding detected, `\n` line
/// ends); None for binary or very big files.
pub fn read_text(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = fs::File::open(path).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_SEARCH_FILE {
        return None;
    }
    // Binary files are recognized from their beginning.
    let mut bytes = vec![0; 8000.min(meta.len() as usize)];
    f.read_exact(&mut bytes).ok()?;
    if is_binary(&bytes) {
        return None;
    }
    f.read_to_end(&mut bytes).ok()?;
    Some(encoding::normalize_eol(encoding::decode(&bytes, encoding::detect(&bytes))).0)
}

/// Part of a line to show for a match at bytes `start..end` of `line`:
/// (snippet, match inside it in characters, column of the match).
pub fn snippet(line: &str, start: usize, end: usize) -> (String, (u32, u32), u32) {
    let before: Vec<char> = line[..start].chars().collect();
    let found: Vec<char> = line[start..end.max(start)].chars().collect();
    let after = &line[end.max(start)..];
    // Some text around the match, without leading blanks.
    let skip = before.iter().take_while(|c| c.is_whitespace()).count().max(before.len().saturating_sub(SNIPPET_BEFORE));
    let mut snippet = String::new();
    if skip > 0 && before[..skip].iter().any(|c| !c.is_whitespace()) {
        snippet.push('…');
    }
    snippet.extend(&before[skip..]);
    let hl_start = snippet.chars().count() as u32;
    snippet.extend(&found);
    let mut tail = after.chars();
    snippet.extend(tail.by_ref().take(SNIPPET_AFTER));
    if tail.next().is_some() {
        snippet.push('…');
    }
    (snippet, (hl_start, found.len() as u32), before.len() as u32)
}

/// Byte ranges of comments and strings in `text` (sorted).
fn comments_and_strings(lang: crate::lang::Lang, text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut state = crate::lexer::initial_state(lang);
    let mut toks = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        state = crate::lexer::lex_line(lang, state, line, &mut toks);
        for t in toks.iter().filter(|t| matches!(t.kind, crate::lexer::Kind::Comment | crate::lexer::Kind::Str)) {
            out.push((offset + t.start as usize, offset + t.end as usize));
        }
        offset += line.len() + 1;
    }
    out
}

/// Searches `q` in the files of `root` (same files as the quick search).
/// With `code_only`, matches inside comments and strings are skipped.
/// Slow: to be run in a thread; `progress.cancel` stops it.
pub fn grep(root: &Path, q: &Query, code_only: bool, progress: &Progress) -> TextResults {
    let paths = scan(root);
    progress.total.store(paths.len(), Ordering::Relaxed);
    let mut res = TextResults::default();
    for i in 0..paths.len() {
        if progress.cancel.load(Ordering::Relaxed) {
            res.cancelled = true;
            break;
        }
        progress.done.store(i + 1, Ordering::Relaxed);
        res.searched = i + 1;
        let rel = paths.get(i);
        let Some(text) = read_text(&root.join(rel)) else { continue };
        let mut file: Option<u32> = None;
        // Comments and strings, computed for files that match.
        let mut skip_ranges: Option<Vec<(usize, usize)>> = None;
        // Line of the last match, to report each line once.
        let (mut line, mut line_start, mut counted_to) = (0u32, 0usize, 0usize);
        let mut last_line = u32::MAX;
        for m in q.re.find_iter(&text) {
            if m.start() == m.end() {
                continue;
            }
            if code_only {
                let ranges = skip_ranges.get_or_insert_with(|| {
                    let head: String = text.chars().take(256).collect();
                    match crate::lang::detect(Some(Path::new(rel)), &head) {
                        crate::lang::Lang::Plain => Vec::new(),
                        lang => comments_and_strings(lang, &text),
                    }
                });
                let i = ranges.partition_point(|r| r.1 <= m.start());
                if ranges.get(i).is_some_and(|r| r.0 <= m.start()) {
                    continue;
                }
            }
            for (j, b) in text.as_bytes()[counted_to..m.start()].iter().enumerate() {
                if *b == b'\n' {
                    line += 1;
                    line_start = counted_to + j + 1;
                }
            }
            counted_to = m.start();
            if line == last_line {
                continue;
            }
            last_line = line;
            let line_end = text[line_start..].find('\n').map_or(text.len(), |e| line_start + e);
            let (snippet, hl, col) = snippet(&text[line_start..line_end], m.start() - line_start, m.end().min(line_end) - line_start);
            let idx = *file.get_or_insert_with(|| {
                res.files.push(rel.to_string());
                (res.files.len() - 1) as u32
            });
            res.matches.push(TextMatch { file: idx, line, col, snippet, hl });
            if res.matches.len() >= MAX_TEXT_MATCHES {
                res.truncated = true;
                return res;
            }
        }
    }
    res
}

// ---------------------------------------------------------------- cache

fn cache_path(root: &Path) -> PathBuf {
    // FNV-1a of the folder path.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    config::base_dir().join("index").join(format!("{h:016x}.paths"))
}

/// Paths saved by the last scan of `root`.
pub fn load_cache(root: &Path) -> Option<Paths> {
    let data = fs::read(cache_path(root)).ok()?;
    let data = String::from_utf8(data).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    Some(Paths::from_blob(data))
}

pub fn save_cache(root: &Path, paths: &Paths) {
    let _ = config::write_atomic(&cache_path(root), paths.data.as_bytes());
}

pub fn delete_cache(root: &Path) {
    let _ = fs::remove_file(cache_path(root));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_dirs() {
        assert!(in_hidden_dir(".git/HEAD"));
        assert!(in_hidden_dir("a/.cache/x"));
        assert!(!in_hidden_dir(".gitignore"));
        assert!(!in_hidden_dir("src/.env"));
        assert!(!in_hidden_dir("src/main.rs"));
    }

    #[test]
    fn ranking() {
        let v: Vec<String> = ["src/twin/x.rs", "src/window.rs", "docs/w/i/n.txt", "src/window/gitui.rs", "è/Wìn.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let paths = Paths::from_vec(&v);
        assert_eq!(paths.len(), 5);
        assert_eq!(paths.get(4), "è/Wìn.txt");
        let found = search(&paths, "win", 10, None);
        assert_eq!(paths.get(found.hits[0].index), "src/window.rs");
        assert_eq!(found.hits.len(), 4);
        // A longer query only looks among the previous matches.
        let narrowed = search(&paths, "wind", 10, Some(&found.all));
        assert_eq!(narrowed.hits.len(), 2);
        let (_, pos) = score("gitui", "src/window/gitui.rs").unwrap();
        assert_eq!(pos, vec![11, 12, 13, 14, 15]);
        assert_eq!(score("w", "è/Wìn.txt").unwrap().1, vec![2]);
        assert!(score("xyz", "src/window.rs").is_none());
        assert_eq!(paths.get(search(&paths, "Git UI", 10, None).hits[0].index), "src/window/gitui.rs");
        assert_eq!(search(&paths, "", 2, None).hits.len(), 2);
    }

    #[test]
    fn grep_folder() {
        use crate::search::Options;
        let dir = std::env::temp_dir().join(format!("macaco-grep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("a.txt"), "uno\n    Ciao mondo, ciao\ntre\r\nciao").unwrap();
        fs::write(dir.join(".git/x"), "ciao").unwrap();
        fs::write(dir.join("bin.dat"), b"ciao\0\0").unwrap();
        let q = Query::new("ciao", Options::default()).unwrap();
        let res = grep(&dir, &q, false, &Progress::default());
        assert_eq!(res.files, vec!["a.txt"]);
        assert_eq!(res.matches.len(), 2);
        let m = &res.matches[0];
        assert_eq!((m.line, m.col), (1, 4));
        assert_eq!(m.snippet, "Ciao mondo, ciao");
        assert_eq!(m.hl, (0, 4));
        assert_eq!(res.matches[1].line, 3);
        // Only code: the match in the comment is skipped.
        fs::write(dir.join("b.rs"), "fn ciao() {}\n// ciao\nlet s = \"ciao\"; ciao();\n").unwrap();
        let res = grep(&dir, &q, true, &Progress::default());
        let in_b: Vec<u32> = res.matches.iter().filter(|m| res.files[m.file as usize] == "b.rs").map(|m| m.line).collect();
        assert_eq!(in_b, vec![0, 2]);
        let m = res.matches.iter().find(|m| res.files[m.file as usize] == "b.rs" && m.line == 2).unwrap();
        assert_eq!(m.col, 16);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_folder() {
        let dir = std::env::temp_dir().join(format!("macaco-finder-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for p in ["a/b.txt", ".git/HEAD", ".cache/x", "c.rs", ".gitignore"] {
            let f = dir.join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(&f, "x").unwrap();
        }
        let mut files = walk(&dir);
        files.retain(|p| !in_hidden_dir(p));
        files.sort();
        assert_eq!(files, vec![".gitignore", "a/b.txt", "c.rs"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
